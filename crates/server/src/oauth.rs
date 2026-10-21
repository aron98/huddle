//! OAuth authorization server (issue #9, ADR 013): the RFC 8628 device
//! flow, the token and revocation endpoints, and the browser approval
//! page that grants or denies a device.
//!
//! huddle is its own authorization server — no external IdP is
//! involved, and this module never touches [`crate::identity`] beyond
//! reading the session cookie that [`crate::auth`] already issued
//! (ADR 003 stays intact).
//!
//! Credential discipline mirrors the existing code:
//!
//! * Device codes and refresh tokens are random 32-byte values; only
//!   their SHA-256 hashes are stored. A leaked database authorizes
//!   nothing.
//! * Access tokens are stateless HMAC-signed values
//!   (`at1.<organizer-id>.<scopes>.<expiry>.<hmac>`), the same
//!   construction as session cookies — short TTL, no row per token.
//! * Refresh tokens live in the database so the dashboard can list and
//!   revoke them; revocation is an immediate row delete.
//!
//! The browser approval page (`/oauth/device`) is server-rendered
//! plain HTML — the Leptos bundle is not loaded for it, so no CSP hash
//! churn and no JS. A stateless CSRF token (an HMAC over the organizer
//! and user code, recomputed server-side on submit) guards the
//! approve/deny POSTs.

use axum::Json;
use axum::extract::{ConnectInfo, Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use hmac::Mac;
use rand::RngExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::net::SocketAddr;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use uuid::Uuid;

use huddle_protocol::{
    ApiError, AuthorizationDto, DEVICE_GRANT_TYPE, DeviceAuthorizationResponse, ErrorCode,
    OAuthError, OAuthTokenResponse, Scope, scopes_string,
};

use crate::auth::{self, hex, hmac_for};
use crate::identity::Organizer;
use crate::meetings::{from_text, to_text};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Lifetimes and pacing (constants, not config: one deployment = one policy)
// ---------------------------------------------------------------------------

/// How long a device request may sit unapproved.
const DEVICE_REQUEST_TTL_MIN: i64 = 10;
/// Access token TTL: short on purpose, refresh is cheap.
const ACCESS_TOKEN_TTL_MIN: i64 = 30;
/// Refresh token TTL: the "long-lived" half of the pair.
const REFRESH_TOKEN_TTL_DAYS: i64 = 90;
/// Default minimum seconds between token polls (RFC 8628 `interval`).
const POLL_INTERVAL_SEC: i64 = 5;
/// How much a `slow_down` bumps the stored interval by.
const POLL_INTERVAL_STEP_SEC: i64 = 5;
/// The user-code alphabet: unreserved characters only, no `0/O/1/I`
/// (RFC 8628 §5.2's readability advice).
const USER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Domain-separation prefix for the approval-page CSRF HMAC.
const CSRF_CONTEXT: &str = "oauth-device-csrf";

fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

// ---------------------------------------------------------------------------
// Bearer principals (the requestor concept)
// ---------------------------------------------------------------------------

/// Who is making an API request: a browser session (full access) or a
/// bearer access token (scoped). `meetings:write` does not imply
/// `meetings:read` — scopes are checked exactly.
pub struct Requestor {
    pub organizer: Organizer,
    /// The scopes a bearer token carries; empty for sessions.
    pub scopes: Vec<Scope>,
    /// Session cookies are the dashboard: every scope, implicitly.
    pub via_session: bool,
}

impl Requestor {
    /// True when this requestor may act under `scope`. Sessions act
    /// under every scope; bearer tokens under the ones they were
    /// granted and no others.
    pub fn allows(&self, scope: Scope) -> bool {
        self.via_session || self.scopes.contains(&scope)
    }
}

/// Resolve the requestor: the session cookie wins, then a bearer
/// access token. `None` is an anonymous request.
pub async fn requestor(state: &AppState, headers: &HeaderMap) -> Option<Requestor> {
    if let Some(organizer) = auth::session_organizer(state, headers) {
        return Some(Requestor {
            organizer,
            scopes: Vec::new(),
            via_session: true,
        });
    }
    bearer_requestor(state, headers).await
}

/// Validate the `Authorization: Bearer` header into a scoped principal.
async fn bearer_requestor(state: &AppState, headers: &HeaderMap) -> Option<Requestor> {
    let token = bearer_value(headers)?;
    let (id, scopes) =
        authenticate_access_token(state.session_key(), token, OffsetDateTime::now_utc())?;
    // The organizer must still exist — deleting an organizer row
    // retires their tokens.
    let row = sqlx::query("SELECT email FROM organizers WHERE id = ?1")
        .bind(id.to_string())
        .fetch_optional(state.pool())
        .await
        .ok()??;
    let email: String = row.try_get("email").ok()?;
    Some(Requestor {
        organizer: Organizer { id, email },
        scopes,
        via_session: false,
    })
}

fn bearer_value(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

// ---------------------------------------------------------------------------
// Access tokens: stateless, HMAC-signed, cookie-shaped
// ---------------------------------------------------------------------------

/// Mint `at1.<organizer-id>.<scope+scope>.<unix-expiry>` with an
/// HMAC-SHA256 signature over that payload.
fn issue_access_token(
    key: &[u8],
    organizer_id: Uuid,
    scopes: &[Scope],
    expires_at: OffsetDateTime,
) -> String {
    let payload = format!(
        "at1.{}.{}.{}",
        organizer_id,
        scopes
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("+"),
        expires_at.unix_timestamp()
    );
    let mut mac = hmac_for(key);
    mac.update(payload.as_bytes());
    format!("{payload}.{}", hex(&mac.finalize().into_bytes()))
}

/// Verify an access token and return the organizer and scopes it
/// carries. `None` for tampered, malformed, or expired values.
fn authenticate_access_token(
    key: &[u8],
    value: &str,
    now: OffsetDateTime,
) -> Option<(Uuid, Vec<Scope>)> {
    let (signed, signature) = value.rsplit_once('.')?;
    let (head, expiry_str) = signed.rsplit_once('.')?;
    let expiry: i64 = expiry_str.parse().ok()?;
    if expiry <= now.unix_timestamp() {
        return None;
    }
    let rest = head.strip_prefix("at1.")?;
    // The organizer id is a fixed-shape UUID and scopes never contain
    // dots, so the first dot bounds the id; scopes are '+'-joined.
    let (id_str, scopes_str) = rest.split_once('.')?;
    let id: Uuid = id_str.parse().ok()?;
    let scopes: Vec<Scope> = scopes_str
        .split('+')
        .map(Scope::parse)
        .collect::<Option<Vec<_>>>()?;
    if scopes.is_empty() {
        return None;
    }
    let mut mac = hmac_for(key);
    mac.update(signed.as_bytes());
    let expected = hex(&mac.finalize().into_bytes());
    let valid = signature.len() == expected.len()
        && bool::from(signature.as_bytes().ct_eq(expected.as_bytes()));
    valid.then_some((id, scopes))
}

// ---------------------------------------------------------------------------
// Error responses
// ---------------------------------------------------------------------------

/// The RFC 6749 §5.2-shaped error body with its status code.
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        Json(OAuthError {
            error: error.into(),
            error_description: Some(description.into()),
        }),
    )
        .into_response()
}

fn rate_limited() -> Response {
    oauth_error(
        StatusCode::TOO_MANY_REQUESTS,
        "slow_down",
        "too many requests",
    )
}

fn storage_error(context: &str) -> Response {
    tracing::error!("oauth storage failure: {context}");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "internal error",
    )
}

/// The `ApiError` envelope the REST API uses (authorizations endpoints
/// speak it, not the OAuth error shape — they are dashboard routes).
fn api_error(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    (
        status,
        Json(ApiError {
            code,
            message: message.into(),
        }),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/oauth/device/register
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RegisterForm {
    client_name: Option<String>,
    scope: Option<String>,
}

/// RFC 8628 §3.1: mint a device code and user code for the CLI to
/// poll with. First-party only — no client_id or client secret; the
/// approval page shows the human exactly which client and scopes they
/// are about to trust, and they decide from there.
pub async fn register(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<RegisterForm>,
) -> Response {
    if !state.rate_limiter().allow(addr.ip()) {
        return rate_limited();
    }

    let client_name = form
        .client_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("huddle CLI")
        .chars()
        .take(100)
        .collect::<String>();
    let scopes = match form.scope.as_deref() {
        None | Some("") => Scope::ALL.to_vec(),
        Some(raw) => match Scope::parse_list(raw) {
            Some(scopes) => scopes,
            None => return oauth_error(StatusCode::BAD_REQUEST, "invalid_scope", "unknown scope"),
        },
    };

    let device_code: [u8; 32] = rand::rng().random();
    let device_code = hex(&device_code);
    let user_code = mint_user_code();
    let now = OffsetDateTime::now_utc();
    let expires_at = to_text(now + time::Duration::minutes(DEVICE_REQUEST_TTL_MIN));
    let Ok(expires_at) = expires_at else {
        return storage_error("device request expiry");
    };
    let inserted = sqlx::query(
        "INSERT INTO oauth_device_requests \
         (id, device_code_hash, user_code, client_name, scopes, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(hash_token(&device_code))
    .bind(&user_code)
    .bind(&client_name)
    .bind(scopes_string(&scopes))
    .bind(expires_at)
    .execute(state.pool())
    .await;
    if let Err(e) = inserted {
        tracing::error!("device request insert failed: {e}");
        return storage_error("device request insert");
    }

    let base = state.config().base_url.trim_end_matches('/');
    let verification_uri = format!("{base}/oauth/device");
    let response = DeviceAuthorizationResponse {
        verification_uri_complete: Some(format!("{verification_uri}?user_code={user_code}")),
        device_code,
        user_code,
        verification_uri,
        expires_in: (DEVICE_REQUEST_TTL_MIN * 60) as u32,
        interval: POLL_INTERVAL_SEC as u32,
    };
    (StatusCode::OK, Json(response)).into_response()
}

/// Eight alphabet characters, formatted `XXXX-XXXX`.
fn mint_user_code() -> String {
    let mut chars = [0u8; 8];
    for slot in chars.iter_mut() {
        *slot = USER_CODE_ALPHABET[rand::rng().random_range(0..USER_CODE_ALPHABET.len())];
    }
    let code = std::str::from_utf8(&chars).unwrap_or_default();
    format!("{}-{}", &code[..4], &code[4..])
}

/// Accept a user code as a human would type it: case-insensitive,
/// ignoring dashes and stray spaces. `None` when it cannot be a code.
fn normalize_user_code(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .to_ascii_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if cleaned.len() != 8 || !cleaned.bytes().all(|b| USER_CODE_ALPHABET.contains(&b)) {
        return None;
    }
    Some(format!("{}-{}", &cleaned[..4], &cleaned[4..]))
}

// ---------------------------------------------------------------------------
// POST /api/oauth/token
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TokenForm {
    grant_type: Option<String>,
    device_code: Option<String>,
    refresh_token: Option<String>,
}

/// RFC 8628 §3.4 / RFC 6749 §3.4: the CLI's single token endpoint.
pub async fn token(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Form(form): Form<TokenForm>,
) -> Response {
    if !state.rate_limiter().allow(addr.ip()) {
        return rate_limited();
    }
    match form.grant_type.as_deref() {
        Some(DEVICE_GRANT_TYPE) => device_grant(&state, form.device_code.as_deref()).await,
        Some("refresh_token") => refresh_grant(&state, form.refresh_token.as_deref()).await,
        Some(_) => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "only the device and refresh_token grants exist",
        ),
        None => oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "grant_type is required",
        ),
    }
}

async fn device_grant(state: &AppState, device_code: Option<&str>) -> Response {
    let Some(device_code) = device_code.map(str::trim).filter(|c| !c.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "device_code is required",
        );
    };
    let row = sqlx::query(
        "SELECT id, client_name, scopes, expires_at, status, organizer_id, \
         last_polled_at, poll_interval_sec FROM oauth_device_requests \
         WHERE device_code_hash = ?1",
    )
    .bind(hash_token(device_code))
    .fetch_optional(state.pool())
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "unknown device code",
            );
        }
        Err(e) => {
            tracing::error!("device request lookup failed: {e}");
            return storage_error("device request lookup");
        }
    };

    let id: String = match row.try_get("id") {
        Ok(id) => id,
        Err(_) => return storage_error("device request id"),
    };
    let expires_at = match row
        .try_get::<String, _>("expires_at")
        .map_err(|e| e.to_string())
        .and_then(|raw| from_text(&raw, "expires_at").map_err(|e| e.to_string()))
    {
        Ok(at) => at,
        Err(_) => return storage_error("device request expiry"),
    };
    let now = OffsetDateTime::now_utc();
    if expires_at <= now {
        // Dead rows are worthless; take them out while we're here.
        let _ = sqlx::query("DELETE FROM oauth_device_requests WHERE id = ?1")
            .bind(&id)
            .execute(state.pool())
            .await;
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "expired_token",
            "the device code has expired",
        );
    }
    let status: String = match row.try_get("status") {
        Ok(status) => status,
        Err(_) => return storage_error("device request status"),
    };
    match status.as_str() {
        "denied" => {
            let _ = sqlx::query("DELETE FROM oauth_device_requests WHERE id = ?1")
                .bind(&id)
                .execute(state.pool())
                .await;
            oauth_error(
                StatusCode::BAD_REQUEST,
                "access_denied",
                "the authorization request was denied",
            )
        }
        "approved" => grant_approved(state, row, &id, now).await,
        "pending" => authorization_pending(state, row, &id, now).await,
        _ => {
            tracing::error!("device request {id} has unknown status {status}");
            oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "unknown device code",
            )
        }
    }
}

/// A poll against a still-pending request: pace the poller, then send
/// them away until the browser decides.
async fn authorization_pending(
    state: &AppState,
    row: sqlx::sqlite::SqliteRow,
    id: &str,
    now: OffsetDateTime,
) -> Response {
    let interval: i64 = match row.try_get("poll_interval_sec") {
        Ok(sec) => sec,
        Err(_) => return storage_error("poll interval"),
    };
    let paced = row
        .try_get::<Option<String>, _>("last_polled_at")
        .ok()
        .flatten()
        .and_then(|raw| from_text(&raw, "last_polled_at").ok())
        .is_some_and(|last| (now - last).whole_seconds() < interval);
    if paced {
        // Too fast: RFC 8628 — the interval grows by 5 seconds for
        // this and all subsequent polls.
        let _ = sqlx::query(
            "UPDATE oauth_device_requests \
             SET poll_interval_sec = poll_interval_sec + ?1 WHERE id = ?2",
        )
        .bind(POLL_INTERVAL_STEP_SEC)
        .bind(id)
        .execute(state.pool())
        .await;
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "slow_down",
            "poll more slowly: increase the interval by 5 seconds",
        );
    }
    let Ok(last_polled_at) = to_text(now) else {
        return storage_error("poll stamp");
    };
    let _ = sqlx::query("UPDATE oauth_device_requests SET last_polled_at = ?1 WHERE id = ?2")
        .bind(last_polled_at)
        .bind(id)
        .execute(state.pool())
        .await;
    oauth_error(
        StatusCode::BAD_REQUEST,
        "authorization_pending",
        "the approval is still pending",
    )
}

/// The grant completed: claim the request row (exactly-once, atomic on
/// the delete), mint the refresh token, and issue the token pair.
async fn grant_approved(
    state: &AppState,
    row: sqlx::sqlite::SqliteRow,
    id: &str,
    now: OffsetDateTime,
) -> Response {
    let organizer_id: Option<String> = match row.try_get("organizer_id") {
        Ok(id) => id,
        Err(_) => return storage_error("device request organizer"),
    };
    let Some(organizer_id) = organizer_id else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "unknown device code",
        );
    };
    let Ok(organizer_id) = Uuid::parse_str(&organizer_id) else {
        return storage_error("device request organizer id");
    };
    let client_name: String = match row.try_get("client_name") {
        Ok(name) => name,
        Err(_) => return storage_error("device request client name"),
    };
    let scopes_raw: String = match row.try_get("scopes") {
        Ok(raw) => raw,
        Err(_) => return storage_error("device request scopes"),
    };
    let Some(scopes) = Scope::parse_list(&scopes_raw) else {
        return storage_error("device request scopes parse");
    };

    // Claim the grant: the first poll to delete the row wins it, a
    // concurrent replay gets nothing.
    let claimed = sqlx::query("DELETE FROM oauth_device_requests WHERE id = ?1")
        .bind(id)
        .execute(state.pool())
        .await;
    match claimed {
        Ok(result) if result.rows_affected() == 1 => {}
        Ok(_) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "unknown device code",
            );
        }
        Err(e) => {
            tracing::error!("device request claim failed: {e}");
            return storage_error("device request claim");
        }
    }

    // The refresh token: random, stored hashed, revocable by row id.
    let refresh: [u8; 32] = rand::rng().random();
    let refresh_token = hex(&refresh);
    let Ok(created_at) = to_text(now) else {
        return storage_error("refresh token created_at");
    };
    let Ok(expires_at) = to_text(now + time::Duration::days(REFRESH_TOKEN_TTL_DAYS)) else {
        return storage_error("refresh token expiry");
    };
    let inserted = sqlx::query(
        "INSERT INTO oauth_refresh_tokens \
         (id, hash, organizer_id, client_name, scopes, created_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(hash_token(&refresh_token))
    .bind(organizer_id.to_string())
    .bind(&client_name)
    .bind(&scopes_raw)
    .bind(created_at)
    .bind(expires_at)
    .execute(state.pool())
    .await;
    if let Err(e) = inserted {
        tracing::error!("refresh token insert failed: {e}");
        return storage_error("refresh token insert");
    }

    let access_token = issue_access_token(
        state.session_key(),
        organizer_id,
        &scopes,
        now + time::Duration::minutes(ACCESS_TOKEN_TTL_MIN),
    );
    (
        StatusCode::OK,
        Json(OAuthTokenResponse {
            access_token,
            token_type: "Bearer".into(),
            expires_in: (ACCESS_TOKEN_TTL_MIN * 60) as u32,
            refresh_token: Some(refresh_token),
            scope: scopes_string(&scopes),
        }),
    )
        .into_response()
}

/// RFC 6749 §6: a refresh grant issues a fresh access token against the
/// stored credential. No rotation — the same refresh token keeps
/// working until it expires or is revoked.
async fn refresh_grant(state: &AppState, refresh_token: Option<&str>) -> Response {
    let Some(refresh_token) = refresh_token.map(str::trim).filter(|t| !t.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "refresh_token is required",
        );
    };
    let row = sqlx::query(
        "SELECT id, organizer_id, scopes, expires_at FROM oauth_refresh_tokens WHERE hash = ?1",
    )
    .bind(hash_token(refresh_token))
    .fetch_optional(state.pool())
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "unknown, expired, or revoked refresh token",
            );
        }
        Err(e) => {
            tracing::error!("refresh token lookup failed: {e}");
            return storage_error("refresh token lookup");
        }
    };

    let id: String = match row.try_get("id") {
        Ok(id) => id,
        Err(_) => return storage_error("refresh token id"),
    };
    let organizer_id: String = match row.try_get("organizer_id") {
        Ok(id) => id,
        Err(_) => return storage_error("refresh token organizer"),
    };
    let Ok(organizer_id) = Uuid::parse_str(&organizer_id) else {
        return storage_error("refresh token organizer id");
    };
    let scopes_raw: String = match row.try_get("scopes") {
        Ok(raw) => raw,
        Err(_) => return storage_error("refresh token scopes"),
    };
    let Some(scopes) = Scope::parse_list(&scopes_raw) else {
        return storage_error("refresh token scopes parse");
    };
    let expires_at = match row
        .try_get::<String, _>("expires_at")
        .map_err(|e| e.to_string())
        .and_then(|raw| from_text(&raw, "expires_at").map_err(|e| e.to_string()))
    {
        Ok(at) => at,
        Err(_) => return storage_error("refresh token expiry"),
    };
    let now = OffsetDateTime::now_utc();
    if expires_at <= now {
        let _ = sqlx::query("DELETE FROM oauth_refresh_tokens WHERE id = ?1")
            .bind(&id)
            .execute(state.pool())
            .await;
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "unknown, expired, or revoked refresh token",
        );
    }

    let Ok(last_used_at) = to_text(now) else {
        return storage_error("refresh token last_used_at");
    };
    let _ = sqlx::query("UPDATE oauth_refresh_tokens SET last_used_at = ?1 WHERE id = ?2")
        .bind(last_used_at)
        .bind(&id)
        .execute(state.pool())
        .await;

    let access_token = issue_access_token(
        state.session_key(),
        organizer_id,
        &scopes,
        now + time::Duration::minutes(ACCESS_TOKEN_TTL_MIN),
    );
    (
        StatusCode::OK,
        Json(OAuthTokenResponse {
            access_token,
            token_type: "Bearer".into(),
            expires_in: (ACCESS_TOKEN_TTL_MIN * 60) as u32,
            refresh_token: None,
            scope: scopes_string(&scopes),
        }),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/oauth/revoke
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct RevokeForm {
    token: Option<String>,
}

/// RFC 7009, the single-token flavor: delete the refresh row by hash.
/// Always `200` — the response must not reveal whether the token
/// existed (an attacker probing token validity learns nothing).
pub async fn revoke(State(state): State<AppState>, Form(form): Form<RevokeForm>) -> Response {
    if let Some(token) = form
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let _ = sqlx::query("DELETE FROM oauth_refresh_tokens WHERE hash = ?1")
            .bind(hash_token(token))
            .execute(state.pool())
            .await;
    }
    (StatusCode::OK, Json(serde_json::json!({}))).into_response()
}

// ---------------------------------------------------------------------------
// Dashboard: list and revoke authorizations (session-only routes)
// ---------------------------------------------------------------------------

/// GET /api/oauth/authorizations — the active client authorizations:
/// one row per refresh token, newest first. Bearer tokens cannot list
/// authorizations; only the browser dashboard can.
pub async fn authorizations(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(organizer) = auth::session_organizer(&state, &headers) else {
        return api_error(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "sign in first",
        );
    };
    let rows = sqlx::query(
        "SELECT id, client_name, scopes, created_at, last_used_at \
         FROM oauth_refresh_tokens WHERE organizer_id = ?1 ORDER BY created_at DESC",
    )
    .bind(organizer.id.to_string())
    .fetch_all(state.pool())
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("authorization list failed: {e}");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "internal error",
            );
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let dto = authorization_dto(&row);
        let Some(dto) = dto else {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "internal error",
            );
        };
        out.push(dto);
    }
    (StatusCode::OK, Json(out)).into_response()
}

fn authorization_dto(row: &sqlx::sqlite::SqliteRow) -> Option<AuthorizationDto> {
    let id: String = row.try_get("id").ok()?;
    let client_name: String = row.try_get("client_name").ok()?;
    let scopes_raw: String = row.try_get("scopes").ok()?;
    let created_at: String = row.try_get("created_at").ok()?;
    let last_used_at: Option<String> = row.try_get("last_used_at").ok()?;
    Some(AuthorizationDto {
        id: Uuid::parse_str(&id).ok()?,
        client_name,
        scopes: Scope::parse_list(&scopes_raw)?,
        created_at: from_text(&created_at, "created_at").ok()?,
        last_used_at: last_used_at.and_then(|raw| from_text(raw.as_str(), "last_used_at").ok()),
    })
}

/// DELETE /api/oauth/authorizations/{id} — revoke one client: an
/// immediate row delete, so the refresh token dies with it.
pub async fn delete_authorization(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Response {
    let Some(organizer) = auth::session_organizer(&state, &headers) else {
        return api_error(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Unauthorized,
            "sign in first",
        );
    };
    let deleted =
        sqlx::query("DELETE FROM oauth_refresh_tokens WHERE id = ?1 AND organizer_id = ?2")
            .bind(id.to_string())
            .bind(organizer.id.to_string())
            .execute(state.pool())
            .await;
    match deleted {
        Ok(result) if result.rows_affected() == 1 => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "revoked" })),
        )
            .into_response(),
        // Someone else's authorization — or no such row at all; the
        // answer is the same and tells the caller nothing.
        Ok(_) => api_error(
            StatusCode::NOT_FOUND,
            ErrorCode::InvalidMessage,
            "no such authorization",
        ),
        Err(e) => {
            tracing::error!("authorization delete failed: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "internal error",
            )
        }
    }
}

// ---------------------------------------------------------------------------
// The browser approval page: /oauth/device
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DevicePageQuery {
    user_code: Option<String>,
}

/// GET /oauth/device — the human half of the device flow. A page the
/// signed-in organizer types (or pastes) the device's user code into,
/// sees exactly which client and scopes are being requested, and
/// approves or denies. Plain HTML, no scripts — nothing here needs
/// the SPA.
pub async fn device_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<DevicePageQuery>,
) -> Response {
    let Some(organizer) = auth::session_organizer(&state, &headers) else {
        return page(
            StatusCode::UNAUTHORIZED,
            "Sign in required",
            "<p>Sign in to approve or deny a device.</p>\
             <p><a href=\"/login\">Go to the login page</a></p>",
        );
    };

    let Some(user_code) = query.user_code.as_deref().and_then(normalize_user_code) else {
        return page(
            StatusCode::OK,
            "Device approval",
            "<p>Enter the code shown on your device.</p>\
             <form method=\"get\" action=\"/oauth/device\">\
             <input name=\"user_code\" autocomplete=\"off\" \
             placeholder=\"XXXX-XXXX\" required> \
             <button type=\"submit\">Continue</button></form>",
        );
    };

    let row = sqlx::query(
        "SELECT client_name, scopes, status, expires_at \
         FROM oauth_device_requests WHERE user_code = ?1",
    )
    .bind(&user_code)
    .fetch_optional(state.pool())
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => {
            return page(
                StatusCode::NOT_FOUND,
                "Unknown code",
                "<p>That code is unknown or has expired. Check the device and try again.</p>",
            );
        }
        Err(e) => {
            tracing::error!("device page lookup failed: {e}");
            return page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "<p>The server could not read the request. Try again.</p>",
            );
        }
    };

    let expires_at = row
        .try_get::<String, _>("expires_at")
        .ok()
        .and_then(|raw| from_text(&raw, "expires_at").ok());
    if expires_at.is_some_and(|at| at <= OffsetDateTime::now_utc()) {
        return page(
            StatusCode::NOT_FOUND,
            "Expired",
            "<p>That request has expired. Restart the login on the device.</p>",
        );
    }
    let status: String = row.try_get("status").unwrap_or_else(|_| "pending".into());
    if status != "pending" {
        return page(
            StatusCode::OK,
            "Already decided",
            "<p>That request has already been approved or denied.</p>",
        );
    }

    let client_name: String = row.try_get("client_name").unwrap_or_default();
    let scopes_raw: String = row.try_get("scopes").unwrap_or_default();
    let scopes: Vec<String> = scopes_raw.split_whitespace().map(escape_html).collect();
    let csrf = csrf_token(state.session_key(), &organizer.id, &user_code);
    let body = format!(
        "<p>A device calling itself <strong>{}</strong> is asking to \
         access your meetings with these scopes:</p>\
         <ul>{}</ul>\
         <p>Device code: <strong>{}</strong></p>\
         <form method=\"post\" action=\"/oauth/device/approve\">\
         <input type=\"hidden\" name=\"user_code\" value=\"{}\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
         <button type=\"submit\">Approve</button></form>\
         <form method=\"post\" action=\"/oauth/device/deny\">\
         <input type=\"hidden\" name=\"user_code\" value=\"{}\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{}\">\
         <button type=\"submit\">Deny</button></form>",
        escape_html(&client_name),
        scopes
            .iter()
            .map(|s| format!("<li><code>{s}</code></li>"))
            .collect::<Vec<_>>()
            .join(""),
        escape_html(&user_code),
        escape_html(&user_code),
        escape_html(&csrf),
        escape_html(&user_code),
        escape_html(&csrf),
    );
    page(StatusCode::OK, "Approve device?", &body)
}

#[derive(Deserialize)]
pub struct DecideForm {
    user_code: String,
    csrf: String,
}

/// POST /oauth/device/approve — grant the device its scopes.
pub async fn device_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DecideForm>,
) -> Response {
    decide(&state, &headers, &form, "approved").await
}

/// POST /oauth/device/deny — refuse the device.
pub async fn device_deny(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DecideForm>,
) -> Response {
    decide(&state, &headers, &form, "denied").await
}

async fn decide(
    state: &AppState,
    headers: &HeaderMap,
    form: &DecideForm,
    status: &str,
) -> Response {
    let Some(organizer) = auth::session_organizer(state, headers) else {
        return page(
            StatusCode::UNAUTHORIZED,
            "Sign in required",
            "<p>Sign in to approve or deny a device.</p>\
             <p><a href=\"/login\">Go to the login page</a></p>",
        );
    };
    // The CSRF token is an HMAC over this organizer and this user
    // code; recomputing it server-side is the check. The device
    // operator may know the user code, but not the key.
    let expected = csrf_token(state.session_key(), &organizer.id, form.user_code.trim());
    let valid = form.csrf.len() == expected.len()
        && bool::from(form.csrf.as_bytes().ct_eq(expected.as_bytes()));
    if !valid {
        return page(
            StatusCode::FORBIDDEN,
            "Request refused",
            "<p>The request could not be verified. Reload the approval page and try again.</p>",
        );
    }
    let Some(user_code) = normalize_user_code(&form.user_code) else {
        return page(
            StatusCode::NOT_FOUND,
            "Unknown code",
            "<p>That code is unknown or has expired. Check the device and try again.</p>",
        );
    };
    let updated = sqlx::query(
        "UPDATE oauth_device_requests SET status = ?1, organizer_id = ?2 \
             WHERE user_code = ?3 AND status = 'pending'",
    )
    .bind(status)
    .bind(organizer.id.to_string())
    .bind(&user_code)
    .execute(state.pool())
    .await;
    match updated {
        Ok(result) if result.rows_affected() == 1 => {}
        Ok(_) => {
            return page(
                StatusCode::NOT_FOUND,
                "Unknown code",
                "<p>That code is unknown, expired, or already decided.</p>",
            );
        }
        Err(e) => {
            tracing::error!("device decide failed: {e}");
            return page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "<p>The server could not record the decision. Try again.</p>",
            );
        }
    }
    Redirect::to(&format!("/oauth/device/done?result={status}")).into_response()
}

#[derive(Deserialize)]
pub struct DoneQuery {
    result: Option<String>,
}

/// GET /oauth/device/done — the post-decision landing page.
pub async fn device_done(Query(query): Query<DoneQuery>) -> Response {
    let body = if query.result.as_deref() == Some("approved") {
        "<p>The device was approved. You can return to it now.</p>"
    } else {
        "<p>The device was denied. It will stop asking.</p>"
    };
    page(StatusCode::OK, "Device access", body)
}

/// The CSRF token for an approval-page submit.
fn csrf_token(key: &[u8], organizer_id: &Uuid, user_code: &str) -> String {
    let mut mac = hmac_for(key);
    mac.update(format!("{CSRF_CONTEXT}:{organizer_id}:{user_code}").as_bytes());
    hex(&mac.finalize().into_bytes())
}

/// Minimal HTML escaping for the page interpolations.
fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The shared page shell: one inline style block, no scripts, no
/// external anything. The CSP below matches that exactly.
fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!DOCTYPE html>\
         <html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title} — huddle</title>\
         <style>body{{font-family:system-ui,sans-serif;max-width:34em;margin:4rem auto;\
         padding:0 1rem;color:#1a1a1a}}code{{background:#f2f2f2;padding:.1rem .4rem;\
         border-radius:.3rem}}button{{padding:.6rem 1.4rem;margin-top:1rem;\
         border-radius:.4rem;border:1px solid #bbb;background:#f7f7f7;font-size:1rem}}\
         form{{margin-top:1rem}}input{{padding:.6rem;font-size:1rem}}</style>\
         </head><body><h1>{title}</h1>{body}</body></html>"
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'",
            ),
        ],
        html,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes() -> Vec<Scope> {
        Scope::ALL.to_vec()
    }

    #[test]
    fn access_tokens_round_trip_until_they_expire() {
        let key = b"test-secret".as_slice();
        let organizer_id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();
        let token = issue_access_token(
            key,
            organizer_id,
            &scopes(),
            now + time::Duration::minutes(1),
        );

        let (id, granted) =
            authenticate_access_token(key, &token, now).expect("valid token authenticates");
        assert_eq!(id, organizer_id);
        assert_eq!(granted, scopes());

        // Expired tokens are rejected.
        assert!(authenticate_access_token(key, &token, now + time::Duration::minutes(2)).is_none());
    }

    #[test]
    fn tampered_access_tokens_are_rejected() {
        let key = b"test-secret".as_slice();
        let now = OffsetDateTime::now_utc();
        let token = issue_access_token(
            key,
            Uuid::new_v4(),
            &scopes(),
            now + time::Duration::minutes(10),
        );

        // A different key does not validate.
        assert!(authenticate_access_token(b"other".as_slice(), &token, now).is_none());

        // Tampering with the payload breaks the signature: swap the
        // organizer id but keep the signature.
        let (signed, signature) = token.rsplit_once('.').unwrap();
        let (head, expiry) = signed.rsplit_once('.').unwrap();
        let rest = head.strip_prefix("at1.").unwrap();
        let (_, scopes_str) = rest.split_once('.').unwrap();
        let forged = format!("at1.{}.{scopes_str}.{expiry}.{signature}", Uuid::new_v4());
        assert!(authenticate_access_token(key, &forged, now).is_none());

        // Truncated or malformed values never authenticate.
        assert!(authenticate_access_token(key, "at1", now).is_none());
        assert!(authenticate_access_token(key, "", now).is_none());
        // A token with no scopes carries nothing — reject.
        let empty = issue_access_token(key, Uuid::new_v4(), &[], now + time::Duration::minutes(1));
        assert!(authenticate_access_token(key, &empty, now).is_none());
    }

    #[test]
    fn user_codes_normalize_like_humans_type() {
        assert_eq!(
            normalize_user_code("abcd-2345").as_deref(),
            Some("ABCD-2345")
        );
        assert_eq!(
            normalize_user_code(" abCd2345 ").as_deref(),
            Some("ABCD-2345")
        );
        // The alphabet has no 0, O, 1, or I — those are invalid codes.
        assert!(normalize_user_code("ABCD-0000").is_none());
        assert!(normalize_user_code("ABCD-O123").is_none());
        assert!(normalize_user_code("ABCD-1AAA").is_none());
        assert!(normalize_user_code("ABCD-I234").is_none());
        assert!(normalize_user_code("SHORT").is_none());
        assert!(normalize_user_code("ABCD-23456").is_none());

        let minted = mint_user_code();
        assert_eq!(minted.len(), 9, "XXXX-XXXX: {minted}");
        assert_eq!(
            normalize_user_code(&minted).as_deref(),
            Some(minted.as_str())
        );
    }

    #[test]
    fn csrf_tokens_bind_the_organizer_and_code() {
        let key = b"test-secret".as_slice();
        let organizer = Uuid::new_v4();
        let token = csrf_token(key, &organizer, "ABCD-2345");
        assert_eq!(token, csrf_token(key, &organizer, "ABCD-2345"));
        // A different organizer or code yields a different token.
        assert_ne!(token, csrf_token(key, &Uuid::new_v4(), "ABCD-2345"));
        assert_ne!(token, csrf_token(key, &organizer, "VXYZ-6789"));
        assert_ne!(
            token,
            csrf_token(b"other".as_slice(), &organizer, "ABCD-2345")
        );
    }

    #[test]
    fn tokens_are_stored_hashed() {
        // The wire value never reaches the database as-is.
        let token = "abc123";
        assert_eq!(hash_token(token), hex(&Sha256::digest(token.as_bytes())));
        assert_ne!(hash_token(token), token);
    }
}
