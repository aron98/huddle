//! Organizer sessions: HMAC-signed stateless cookies and `/api/auth` routes.
//!
//! The cookie carries `v1.<organizer-id>.<email>.<unix-expiry>` plus an
//! HMAC-SHA256 signature over that payload — no session table, per ADR 003.
//! Revocation is waiting out the (short) TTL; the trade-off is recorded
//! there.

use axum::Json;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use std::net::SocketAddr;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use uuid::Uuid;

use huddle_protocol::{ApiError, ErrorCode};

use crate::identity::{Identity, IdentityError, IdentityProvider, Organizer};
use crate::mailer::{self, MailTransport};
use crate::state::AppState;

/// The browser-visible session cookie name.
pub const COOKIE_NAME: &str = "huddle_session";

/// The browser-binding cookie for an in-flight OIDC login: the callback
/// only accepts a `state` that this browser (and no other) was issued.
/// Short-lived; cleared implicitly when it lapses.
pub const OIDC_LOGIN_COOKIE: &str = "huddle_oidc_login";

// ---------------------------------------------------------------------------
// Cookie format
// ---------------------------------------------------------------------------

/// HMAC-SHA256 over any key length; shared with the OAuth module.
pub(crate) fn hmac_for(key: &[u8]) -> Hmac<Sha256> {
    // HMAC accepts keys of any length, so this cannot fail.
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mac) => mac,
        Err(_) => unreachable!("hmac accepts arbitrary key lengths"),
    }
}

/// Hex encoding, shared with the OAuth module's token machinery.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Sign a session for `organizer`, valid until `expires_at`.
fn issue_cookie(key: &[u8], organizer: &Organizer, expires_at: OffsetDateTime) -> String {
    let payload = format!(
        "v1.{}.{}.{}",
        organizer.id,
        organizer.email,
        expires_at.unix_timestamp()
    );
    let mut mac = hmac_for(key);
    mac.update(payload.as_bytes());
    format!("{payload}.{}", hex(&mac.finalize().into_bytes()))
}

/// Verify a cookie value and return the organizer it carries. `None` for
/// tampered, malformed, or expired cookies.
///
/// Emails may contain dots, so parse from the right: the last two dots
/// bound the signature and expiry, and the uuid's fixed shape separates
/// the id from the email.
fn authenticate_cookie(key: &[u8], value: &str, now: OffsetDateTime) -> Option<Organizer> {
    // `signed` is everything the issuer authenticated: v1, id, email,
    // and expiry — in that order, with the email last before expiry.
    let (signed, signature) = value.rsplit_once('.')?;
    let (head, expiry_str) = signed.rsplit_once('.')?;
    let expiry: i64 = expiry_str.parse().ok()?;
    let id_email = head.strip_prefix("v1.")?;
    let (id_str, email) = id_email.split_once('.')?;
    let id: Uuid = id_str.parse().ok()?;
    let email = email.to_owned();
    if expiry <= now.unix_timestamp() {
        return None;
    }
    let mut mac = hmac_for(key);
    mac.update(signed.as_bytes());
    let expected = hex(&mac.finalize().into_bytes());
    // Constant-time compare; a length mismatch fails before timing even
    // starts, which leaks nothing (signatures are never secret lengths).
    let valid = signature.len() == expected.len()
        && bool::from(signature.as_bytes().ct_eq(expected.as_bytes()));
    valid.then_some(Organizer { id, email })
}

/// The value of cookie `name` in `headers`, if present.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    let prefix = format!("{name}=");
    cookies
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(&prefix))
        .map(str::to_owned)
}

/// The signed-in organizer for a request, if any.
pub fn session_organizer(state: &AppState, headers: &HeaderMap) -> Option<Organizer> {
    let value = cookie_value(headers, COOKIE_NAME)?;
    authenticate_cookie(state.session_key(), &value, OffsetDateTime::now_utc())
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RequestLinkBody {
    email: String,
}

/// POST /api/auth/request-link — issue a magic link for the email.
///
/// Always `200` on the happy path: the response must not reveal whether
/// an address is valid or known. Delivery failures are logged
/// server-side. One exception: per-IP rate limiting answers `429` so
/// the endpoint cannot be used to flood mailboxes (or mine the log).
///
/// When SSO is configured this endpoint is replaced by OIDC and answers
/// `409` pointing at the login route — magic links and SSO are two
/// flavors of one flow, never simultaneous.
pub async fn request_link(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<RequestLinkBody>,
) -> Response {
    if state.identity().is_oidc() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "status": "oidc_configured",
                "login": "/api/auth/oidc/login",
            })),
        )
            .into_response();
    }
    if !state.rate_limiter().allow(addr.ip()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "status": "rate_limited" })),
        )
            .into_response();
    }
    match state
        .identity()
        .start_login(&body.email, OffsetDateTime::now_utc())
        .await
    {
        Ok(token) => {
            let base = state.config().base_url.trim_end_matches('/');
            let link = format!("{base}/api/auth/verify?token={token}");
            if let Err(e) = state
                .mailer()
                .send(mailer::Email::login_link(&body.email, &link))
                .await
            {
                tracing::warn!("magic-link delivery failed: {e}");
            }
        }
        Err(IdentityError::InvalidEmail) => {
            tracing::debug!("login requested for malformed email; ignored");
        }
        Err(e) => tracing::warn!("cannot start login: {e}"),
    }
    (StatusCode::OK, Json(json!({ "status": "sent" }))).into_response()
}

#[derive(Deserialize)]
pub struct VerifyQuery {
    token: String,
}

/// GET /api/auth/verify?token=… — consume the link, set the session
/// cookie, and land the organizer on the dashboard.
pub async fn verify(State(state): State<AppState>, Query(query): Query<VerifyQuery>) -> Response {
    let organizer = match state
        .identity()
        .verify(&query.token, None, OffsetDateTime::now_utc())
        .await
    {
        Ok(organizer) => organizer,
        Err(_) => return unauthorized("the sign-in link is invalid or has expired"),
    };
    land_with_session(&state, organizer)
}

/// Issue the session cookie for `organizer` and land on the dashboard —
/// the shared tail of every successful login, magic link or SSO.
fn land_with_session(state: &AppState, organizer: Organizer) -> Response {
    let config = state.config();
    let expires_at =
        OffsetDateTime::now_utc() + time::Duration::minutes(i64::from(config.session_ttl_min));
    let value = issue_cookie(state.session_key(), &organizer, expires_at);
    let secure = if config.secure_cookies() {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{COOKIE_NAME}={value}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax{secure}",
        config.session_ttl_min * 60
    );

    let mut response = Redirect::to("/meetings").into_response();
    if let Ok(header_value) = header::HeaderValue::from_str(&cookie) {
        response
            .headers_mut()
            .insert(header::SET_COOKIE, header_value);
    }
    response
}

// ---------------------------------------------------------------------------
// OIDC single sign-on (issue #11)
// ---------------------------------------------------------------------------

/// GET /api/auth/oidc/login — redirect the browser to the IdP's
/// authorization endpoint with a freshly minted state, PKCE challenge,
/// and nonce. `404` when SSO is not configured: the route does not
/// exist in that deployment.
pub async fn oidc_login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let Identity::Oidc(provider) = state.identity() else {
        return oidc_not_configured();
    };
    // Each login mints a database row; keep the minting rate-limited
    // like magic links.
    if !state.rate_limiter().allow(addr.ip()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "status": "rate_limited" })),
        )
            .into_response();
    }
    match provider.begin_login(OffsetDateTime::now_utc()).await {
        Ok((url, login_state)) => {
            let secure = if state.config().secure_cookies() {
                "; Secure"
            } else {
                ""
            };
            // Bind this login to this browser: the callback refuses any
            // state that was not issued to the same cookie jar. This is
            // the login-CSRF defense; the state itself doubles as the
            // CSRF token toward the IdP.
            let cookie = format!(
                "{OIDC_LOGIN_COOKIE}={login_state}; Path=/api/auth; Max-Age=600; HttpOnly; SameSite=Lax{secure}"
            );
            let mut response = Redirect::to(&url).into_response();
            if let Ok(header_value) = header::HeaderValue::from_str(&cookie) {
                response
                    .headers_mut()
                    .insert(header::SET_COOKIE, header_value);
            }
            response
        }
        Err(e) => {
            tracing::warn!("cannot start OIDC login: {e}");
            (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    code: ErrorCode::Internal,
                    message: "the identity provider is unreachable or misconfigured".into(),
                }),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct OidcCallbackQuery {
    code: String,
    state: String,
}

/// GET /api/auth/oidc/callback?code=…&state=… — the IdP's redirect
/// target. Verifies the authorization code (PKCE, state, nonce, ID
/// token signature) and, on success, issues exactly the session cookie
/// the magic-link flow does and lands on the same dashboard.
pub async fn oidc_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<OidcCallbackQuery>,
) -> Response {
    if !state.identity().is_oidc() {
        return oidc_not_configured();
    }
    // The state must match the one this browser was issued at /login.
    // States are 256-bit random values, so a plain comparison leaks
    // nothing.
    if cookie_value(&headers, OIDC_LOGIN_COOKIE).as_deref() != Some(query.state.as_str()) {
        return unauthorized("OIDC sign-in failed or has expired");
    }
    match state
        .identity()
        .verify(&query.code, Some(&query.state), OffsetDateTime::now_utc())
        .await
    {
        Ok(organizer) => land_with_session(&state, organizer),
        Err(e) => {
            tracing::warn!("OIDC callback rejected: {e}");
            unauthorized("OIDC sign-in failed or has expired")
        }
    }
}

fn oidc_not_configured() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(ApiError {
            code: ErrorCode::InvalidMessage,
            message: "SSO is not configured on this server".into(),
        }),
    )
        .into_response()
}

/// GET /api/auth/me — the signed-in organizer, or 401. Bearer access
/// tokens (agent credentials) answer too: `whoami` works from the CLI
/// with either credential.
pub async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match crate::oauth::requestor(&state, &headers).await {
        Some(requestor) => (
            StatusCode::OK,
            Json(json!({
                "id": requestor.organizer.id,
                "email": requestor.organizer.email,
            })),
        )
            .into_response(),
        None => unauthorized("no session"),
    }
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError {
            code: ErrorCode::Unauthorized,
            message: message.to_owned(),
        }),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn organizer() -> Organizer {
        Organizer {
            id: Uuid::new_v4(),
            // Dots in both the local part and the domain exercise the
            // right-to-left cookie parsing.
            email: "alice.smith@example.com".into(),
        }
    }

    #[test]
    fn cookies_round_trip_until_they_expire() {
        let key = b"test-secret".as_slice();
        let who = organizer();
        let now = OffsetDateTime::now_utc();
        let cookie = issue_cookie(key, &who, now + time::Duration::minutes(1));
        let authed = authenticate_cookie(key, &cookie, now).expect("valid cookie authenticates");
        assert_eq!(authed.id, who.id);
        assert_eq!(authed.email, who.email);

        // Expired cookies are rejected.
        assert!(authenticate_cookie(key, &cookie, now + time::Duration::minutes(2)).is_none());
    }

    #[test]
    fn tampered_cookies_are_rejected() {
        let key = b"test-secret".as_slice();
        let who = organizer();
        let now = OffsetDateTime::now_utc();
        let cookie = issue_cookie(key, &who, now + time::Duration::minutes(10));

        // A different key does not validate the signature.
        let other_key = b"other-secret".as_slice();
        assert!(authenticate_cookie(other_key, &cookie, now).is_none());

        // Tampering with the payload breaks the signature: swap the email.
        let (payload, signature) = cookie.rsplit_once('.').unwrap();
        let (prefix, expiry) = payload.rsplit_once('.').unwrap();
        let id_email = prefix.strip_prefix("v1.").unwrap();
        let (id, _email) = id_email.split_once('.').unwrap();
        let forged = format!("v1.{id}.mallory@example.com.{expiry}.{signature}");
        assert!(authenticate_cookie(key, &forged, now).is_none());

        // Truncated or malformed values never authenticate.
        assert!(authenticate_cookie(key, "v1", now).is_none());
        assert!(authenticate_cookie(key, "", now).is_none());
    }
}
