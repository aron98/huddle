//! Organizer sessions: HMAC-signed stateless cookies and `/api/auth` routes.
//!
//! The cookie carries `v1.<organizer-id>.<email>.<unix-expiry>` plus an
//! HMAC-SHA256 signature over that payload — no session table, per ADR 003.
//! Revocation is waiting out the (short) TTL; the trade-off is recorded
//! there.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use uuid::Uuid;

use huddle_protocol::{ApiError, ErrorCode};

use crate::identity::{IdentityError, IdentityProvider, Organizer};
use crate::mailer;
use crate::state::AppState;

/// The browser-visible session cookie name.
pub const COOKIE_NAME: &str = "huddle_session";

// ---------------------------------------------------------------------------
// Cookie format
// ---------------------------------------------------------------------------

fn hmac_for(key: &[u8]) -> Hmac<Sha256> {
    // HMAC accepts keys of any length, so this cannot fail.
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mac) => mac,
        Err(_) => unreachable!("hmac accepts arbitrary key lengths"),
    }
}

fn hex(bytes: &[u8]) -> String {
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

/// The signed-in organizer for a request, if any.
pub fn session_organizer(state: &AppState, headers: &HeaderMap) -> Option<Organizer> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    let prefix = format!("{COOKIE_NAME}=");
    let value = cookies
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix(&prefix))?;
    authenticate_cookie(state.session_key(), value, OffsetDateTime::now_utc())
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
/// Always `200`: the response must not reveal whether an address is
/// valid or known. Delivery failures are logged server-side.
pub async fn request_link(
    State(state): State<AppState>,
    Json(body): Json<RequestLinkBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state
        .identity()
        .start_login(&body.email, OffsetDateTime::now_utc())
        .await
    {
        Ok(token) => {
            let base = state.config().base_url.trim_end_matches('/');
            let link = format!("{base}/api/auth/verify?token={token}");
            if let Err(e) =
                mailer::send_login_link(state.config().smtp.as_ref(), &body.email, &link).await
            {
                tracing::warn!("magic-link delivery failed: {e}");
            }
        }
        Err(IdentityError::InvalidEmail) => {
            tracing::debug!("login requested for malformed email; ignored");
        }
        Err(e) => tracing::warn!("cannot start login: {e}"),
    }
    (StatusCode::OK, Json(json!({ "status": "sent" })))
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
        .verify(&query.token, OffsetDateTime::now_utc())
        .await
    {
        Ok(organizer) => organizer,
        Err(_) => return unauthorized("the sign-in link is invalid or has expired"),
    };

    let config = state.config();
    let expires_at =
        OffsetDateTime::now_utc() + time::Duration::minutes(i64::from(config.session_ttl_min));
    let value = issue_cookie(state.session_key(), &organizer, expires_at);
    let secure = if config.base_url.starts_with("https://") {
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

/// GET /api/auth/me — the signed-in organizer, or 401.
pub async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match session_organizer(&state, &headers) {
        Some(organizer) => (
            StatusCode::OK,
            Json(json!({ "id": organizer.id, "email": organizer.email })),
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
