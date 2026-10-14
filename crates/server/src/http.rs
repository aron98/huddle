//! HTTP router: API routes plus the embedded web UI.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde_json::json;
use time::OffsetDateTime;

use huddle_protocol::{
    ApiError, CreateMeetingRequest, ErrorCode, MeetingResponse, UpdateMeetingRequest,
};

use crate::auth;
use crate::ics;
use crate::meetings::{self, MeetingError};
use crate::metrics;
use crate::session;
use crate::state::AppState;

/// The web UI bundle. In debug builds rust-embed reads from disk; in release
/// it is baked into the binary (see ADR 004). `Config::web_dir` overrides it
/// at runtime so `trunk watch` output can be served without rebuilding.
#[derive(RustEmbed)]
#[folder = "assets/"]
struct Ui;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/meetings", post(create_meeting))
        .route("/api/meetings/mine", get(my_meetings))
        .route(
            "/api/meetings/{code}",
            get(get_meeting)
                .patch(update_meeting)
                .delete(cancel_meeting),
        )
        .route("/api/meetings/{code}/ics", get(meeting_ics))
        .route("/api/auth/request-link", post(auth::request_link))
        .route("/api/auth/verify", get(auth::verify))
        .route("/api/auth/me", get(auth::me))
        .route("/metrics", get(metrics_scrape))
        .route("/ws", get(session::ws))
        .fallback(fallback)
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn create_meeting(
    state: State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateMeetingRequest>,
) -> Result<(StatusCode, Json<MeetingResponse>), ApiErr> {
    // A logged-in organizer owns what they create; anonymous meetings
    // stay open (the link is the only capability, by design).
    let organizer = auth::session_organizer(&state, &headers);
    let meeting = meetings::create(
        state.pool(),
        &req,
        organizer.map(|o| o.id),
        OffsetDateTime::now_utc(),
    )
    .await
    .map_err(ApiErr::from_meeting_err)?;
    Ok((
        StatusCode::CREATED,
        Json(MeetingResponse {
            joinability: meetings::joinability(&meeting, state.config(), OffsetDateTime::now_utc()),
            meeting: meeting.into(),
        }),
    ))
}

async fn get_meeting(
    state: State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<MeetingResponse>, ApiErr> {
    let meeting = meetings::get_by_code(state.pool(), &code)
        .await
        .map_err(ApiErr::internal)?
        .ok_or(ApiErr::not_found())?;
    Ok(Json(MeetingResponse {
        joinability: meetings::joinability(&meeting, state.config(), OffsetDateTime::now_utc()),
        meeting: meeting.into(),
    }))
}

async fn update_meeting(
    state: State<AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
    Json(req): Json<UpdateMeetingRequest>,
) -> Result<Json<MeetingResponse>, ApiErr> {
    require_owner(&state, &headers, &code).await?;
    let meeting = meetings::update_by_code(state.pool(), &code, req)
        .await
        .map_err(ApiErr::from_meeting_err)?
        .ok_or_else(ApiErr::not_found)?;
    Ok(Json(MeetingResponse {
        joinability: meetings::joinability(&meeting, state.config(), OffsetDateTime::now_utc()),
        meeting: meeting.into(),
    }))
}

async fn cancel_meeting(
    state: State<AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> Result<Json<MeetingResponse>, ApiErr> {
    require_owner(&state, &headers, &code).await?;
    let meeting = meetings::set_cancelled(state.pool(), &code, true)
        .await
        .map_err(ApiErr::internal)?
        .ok_or_else(ApiErr::not_found)?;
    Ok(Json(MeetingResponse {
        joinability: meetings::joinability(&meeting, state.config(), OffsetDateTime::now_utc()),
        meeting: meeting.into(),
    }))
}

/// The organizer's dashboard: every meeting they created, soonest first.
async fn my_meetings(
    state: State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<MeetingResponse>>, ApiErr> {
    let Some(organizer) = auth::session_organizer(&state, &headers) else {
        return Err(ApiErr::unauthorized());
    };
    let meetings = meetings::list_for_organizer(state.pool(), organizer.id)
        .await
        .map_err(ApiErr::internal)?;
    let now = OffsetDateTime::now_utc();
    let responses = meetings
        .into_iter()
        .map(|meeting| MeetingResponse {
            joinability: meetings::joinability(&meeting, state.config(), now),
            meeting: meeting.into(),
        })
        .collect();
    Ok(Json(responses))
}

/// Prometheus scrape endpoint: live rooms, peers, bitrates, loss, RTT.
///
/// Open by default (Prometheus cannot send cookies, and deployments
/// gate scrapes at the network layer); `stats_token` opts into a
/// bearer-token check for setups where the endpoint is more exposed.
async fn metrics_scrape(state: State<AppState>, headers: HeaderMap) -> Result<Response, ApiErr> {
    if let Some(expected) = state.config().stats_token.as_deref() {
        let provided = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !provided.is_some_and(|p| token_matches(p, expected)) {
            return Err(ApiErr::unauthorized());
        }
    }
    let report = state.engine().stats().await.map_err(|_| {
        tracing::error!("engine went away while serving /metrics");
        ApiErr(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError {
                code: ErrorCode::Internal,
                message: "internal error".into(),
            },
        )
    })?;
    Ok((
        [(header::CONTENT_TYPE, metrics::CONTENT_TYPE)],
        metrics::render(&report),
    )
        .into_response())
}

/// Constant-time bearer-token comparison (same discipline as cookies).
fn token_matches(provided: &str, expected: &str) -> bool {
    use subtle::ConstantTimeEq;
    provided.len() == expected.len() && bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
}

/// Download one meeting as an iCalendar file.
async fn meeting_ics(state: State<AppState>, Path(code): Path<String>) -> Result<Response, ApiErr> {
    let meeting = meetings::get_by_code(state.pool(), &code)
        .await
        .map_err(ApiErr::internal)?
        .ok_or_else(ApiErr::not_found)?;
    let body = ics::build_ics(&meeting, &state.config().base_url).map_err(|e| {
        tracing::error!(error = %e, "ics generation failure");
        ApiErr(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError {
                code: ErrorCode::Internal,
                message: "internal error".into(),
            },
        )
    })?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/calendar; charset=utf-8")],
        body,
    )
        .into_response())
}

/// Meetings owned by a logged-in organizer can only be changed by that
/// organizer; anonymous meetings stay open to anyone with the code.
async fn require_owner(state: &AppState, headers: &HeaderMap, code: &str) -> Result<(), ApiErr> {
    let Some(meeting) = meetings::get_by_code(state.pool(), code)
        .await
        .map_err(ApiErr::internal)?
    else {
        // Let the caller's own lookup produce the 404.
        return Ok(());
    };
    if let Some(owner) = meeting.organizer_id {
        let session = auth::session_organizer(state, headers);
        if session.as_ref().map(|o| o.id) != Some(owner) {
            return Err(ApiErr::forbidden());
        }
    }
    Ok(())
}

/// The JSON error envelope all API failures use.
struct ApiErr(StatusCode, ApiError);

impl ApiErr {
    fn not_found() -> Self {
        Self(
            StatusCode::NOT_FOUND,
            ApiError {
                code: ErrorCode::MeetingNotFound,
                message: "no meeting with that code".into(),
            },
        )
    }

    fn unauthorized() -> Self {
        Self(
            StatusCode::UNAUTHORIZED,
            ApiError {
                code: ErrorCode::Unauthorized,
                message: "sign in first".into(),
            },
        )
    }

    fn forbidden() -> Self {
        Self(
            StatusCode::FORBIDDEN,
            ApiError {
                code: ErrorCode::Unauthorized,
                message: "this meeting belongs to another organizer".into(),
            },
        )
    }

    fn from_meeting_err(e: MeetingError) -> Self {
        match e {
            MeetingError::Invalid => Self(
                StatusCode::BAD_REQUEST,
                ApiError {
                    code: ErrorCode::InvalidMessage,
                    message: "invalid meeting fields".into(),
                },
            ),
            MeetingError::NotFound => Self::not_found(),
            e => Self::internal(e),
        }
    }

    fn internal(e: MeetingError) -> Self {
        tracing::error!(error = %e, "meeting storage failure");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError {
                code: ErrorCode::Internal,
                message: "internal error".into(),
            },
        )
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

async fn fallback(state: State<AppState>, uri: Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") {
        return api_not_found();
    }
    serve_asset(&state, path).await
}

fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": { "code": "invalid_message", "message": "no such API route" }
        })),
    )
        .into_response()
}

/// Serve a UI asset. Unknown non-API paths fall back to `index.html` so
/// client-side routes like `/m/{code}` work.
async fn serve_asset(state: &AppState, path: &str) -> Response {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return asset_response(state, "index.html").await;
    }

    // Path traversal guard: no segment may be `..`.
    if path.split('/').any(|seg| seg == "..") {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }

    match asset_response(state, path).await {
        response if response.status() != StatusCode::NOT_FOUND => response,
        // SPA fallback for client-side routes.
        _ => asset_response(state, "index.html").await,
    }
}

async fn asset_response(state: &AppState, path: &str) -> Response {
    // Runtime override directory wins, then the embedded bundle.
    if let Some(dir) = &state.config().web_dir {
        let full = dir.join(path);
        if let Ok(bytes) = tokio::fs::read(&full).await {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type(path))],
                bytes,
            )
                .into_response();
        }
    }

    match Ui::get(path) {
        Some(file) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type(path))],
            file.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or_default();
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn test_state() -> AppState {
        let config = Arc::new(crate::config::Config {
            media_port: 0,
            // Tests talk loopback-to-loopback; detection is machine-dependent.
            public_ip: Some(std::net::IpAddr::from([127, 0, 0, 1])),
            ..crate::config::Config::default()
        });
        let (engine, _media_addr) = crate::engine::spawn(&config).unwrap();
        AppState::new(config, crate::db::test_pool().await, engine)
    }

    #[tokio::test]
    async fn health_is_ok() {
        let response = router(test_state().await)
            .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, r#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn root_serves_embedded_index() {
        let response = router(test_state().await)
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_owned();
        assert!(content_type.starts_with("text/html"));
    }

    #[tokio::test]
    async fn unknown_api_route_is_json_404() {
        let response = router(test_state().await)
            .oneshot(Request::get("/api/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("\"error\""), "got: {text}");
    }

    #[tokio::test]
    async fn spa_routes_fall_back_to_index() {
        let response = router(test_state().await)
            .oneshot(
                Request::get("/m/H7KD-PM2X-QR4T")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn traversal_is_rejected() {
        let response = router(test_state().await)
            .oneshot(Request::get("/../Cargo.toml").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // --- meetings API -----------------------------------------------------

    async fn create_meeting_via(router_app: Router, title: &str) -> (StatusCode, String) {
        let now = time::OffsetDateTime::now_utc();
        let body = serde_json::json!({
            "title": title,
            "starts_at": (now + time::Duration::minutes(10)).format(&time::format_description::well_known::Rfc3339).unwrap(),
            "ends_at": (now + time::Duration::minutes(70)).format(&time::format_description::well_known::Rfc3339).unwrap(),
        });
        let response = router_app
            .oneshot(
                Request::post("/api/meetings")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let text = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&text).into_owned())
    }

    #[tokio::test]
    async fn meeting_crud_round_trip() {
        let state = test_state().await;
        let app = router(state.clone());

        // Create
        let (status, body) = create_meeting_via(app.clone(), "Standup").await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        let created: MeetingResponse = serde_json::from_str(&body).unwrap();
        assert_eq!(created.meeting.title, "Standup");
        assert!(matches!(
            created.joinability,
            huddle_protocol::Joinability::NotYet { .. }
        ));

        // Get
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/api/meetings/{}", created.meeting.code))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let fetched: MeetingResponse =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(fetched.meeting.id, created.meeting.id);

        // Patch
        let patch = serde_json::json!({ "title": "Renamed" });
        let response = app
            .clone()
            .oneshot(
                Request::patch(format!("/api/meetings/{}", created.meeting.code))
                    .header("content-type", "application/json")
                    .body(Body::from(patch.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let updated: MeetingResponse =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(updated.meeting.title, "Renamed");

        // Delete (cancel)
        let response = app
            .oneshot(
                Request::delete(format!("/api/meetings/{}", created.meeting.code))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cancelled: MeetingResponse =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(cancelled.meeting.cancelled);
        assert!(matches!(
            cancelled.joinability,
            huddle_protocol::Joinability::Cancelled
        ));
    }

    #[tokio::test]
    async fn create_meeting_rejects_invalid_json() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::post("/api/meetings")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // axum's Json extractor: 400 for malformed syntax, 422 when the JSON
        // is valid but doesn't deserialize into the request type.
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn unknown_meeting_is_404() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::get("/api/meetings/ZZZZ-ZZZZ-ZZZZ")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body).into_owned();
        assert!(text.contains("meeting_not_found"), "got: {text}");
    }

    #[tokio::test]
    async fn auth_flow_issues_session_and_reports_me() {
        use crate::identity::IdentityProvider;

        let state = test_state().await;
        let token = state
            .identity()
            .start_login("route@example.com", OffsetDateTime::now_utc())
            .await
            .unwrap();

        let response = router(state.clone())
            .oneshot(
                Request::get(format!("/api/auth/verify?token={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("verify sets the session cookie")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.starts_with("huddle_session="), "got: {cookie}");
        let pair = cookie.split(';').next().unwrap().to_owned();

        let response = router(state)
            .oneshot(
                Request::get("/api/auth/me")
                    .header(header::COOKIE, pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body).into_owned();
        assert!(text.contains("route@example.com"), "got: {text}");
    }

    #[tokio::test]
    async fn request_link_succeeds_for_any_email_shape() {
        let app = router(test_state().await);
        for email in ["someone@example.com", "not-an-email"] {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/auth/request-link")
                        .header("content-type", "application/json")
                        .body(Body::from(format!(r#"{{"email":"{email}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            // Always 200: responses must not reveal which addresses are valid.
            assert_eq!(response.status(), StatusCode::OK, "email: {email}");
        }
    }

    #[tokio::test]
    async fn me_without_session_is_401() {
        let app = router(test_state().await);
        let response = app
            .oneshot(Request::get("/api/auth/me").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&body).into_owned();
        assert!(text.contains("unauthorized"), "got: {text}");
    }

    // --- ownership, dashboard, ICS -----------------------------------------

    /// Log `email` in (via the verify route, like a browser following the
    /// magic link) and return its `Cookie` header value.
    async fn session_cookie_for(state: &AppState, app: &Router, email: &str) -> String {
        use crate::identity::IdentityProvider;

        let token = state
            .identity()
            .start_login(email, OffsetDateTime::now_utc())
            .await
            .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/api/auth/verify?token={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("verify sets the session cookie")
            .to_str()
            .unwrap()
            .to_owned();
        cookie.split(';').next().unwrap().to_owned()
    }

    #[tokio::test]
    async fn owned_meetings_require_their_organizer() {
        let state = test_state().await;
        let app = router(state.clone());
        let alice = session_cookie_for(&state, &app, "alice@example.com").await;
        let bob = session_cookie_for(&state, &app, "bob@example.com").await;

        // Alice creates a meeting while logged in: she owns it.
        let now = OffsetDateTime::now_utc();
        let meeting = serde_json::json!({
            "title": "Alice's sync",
            "starts_at": (now + time::Duration::minutes(10)).format(&time::format_description::well_known::Rfc3339).unwrap(),
            "ends_at": (now + time::Duration::minutes(70)).format(&time::format_description::well_known::Rfc3339).unwrap(),
        });
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/meetings")
                    .header(header::COOKIE, &alice)
                    .header("content-type", "application/json")
                    .body(Body::from(meeting.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let created: MeetingResponse =
            serde_json::from_slice(&bytes).expect("create returns a meeting");
        let code = created.meeting.code.clone();

        // The dashboard lists it for Alice, but not for Bob or the anonymous
        // request.
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/meetings/mine")
                    .header(header::COOKIE, &alice)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let mine: Vec<MeetingResponse> = serde_json::from_slice(&bytes).unwrap();
        assert!(mine.iter().any(|m| m.meeting.code == code));

        let response = app
            .clone()
            .oneshot(
                Request::get("/api/meetings/mine")
                    .header(header::COOKIE, &bob)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let theirs: Vec<MeetingResponse> = serde_json::from_slice(&bytes).unwrap();
        assert!(theirs.is_empty(), "bob must not see alice's meetings");

        // Only Alice may change or cancel it.
        let patch = serde_json::json!({ "title": "Renamed by alice" });
        let response = app
            .clone()
            .oneshot(
                Request::patch(format!("/api/meetings/{code}"))
                    .header(header::COOKIE, &alice)
                    .header("content-type", "application/json")
                    .body(Body::from(patch.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        for (label, cookie) in [("anonymous", None), ("bob", Some(&bob))] {
            let mut request = Request::patch(format!("/api/meetings/{code}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{ "title": "Hijacked" }"#))
                .unwrap();
            if let Some(cookie) = cookie {
                request
                    .headers_mut()
                    .insert(header::COOKIE, cookie.parse().unwrap());
            }
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{label} must not update alice's meeting"
            );
        }
    }

    #[tokio::test]
    async fn mine_requires_a_session() {
        let app = router(test_state().await);
        let response = app
            .oneshot(
                Request::get("/api/meetings/mine")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn anonymous_meetings_stay_open() {
        let state = test_state().await;
        let app = router(state.clone());
        // No session at creation: the link is the only capability.
        let (status, body) = create_meeting_via(app.clone(), "Open standup").await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        let created: MeetingResponse = serde_json::from_str(&body).unwrap();
        let code = created.meeting.code;

        let patch = serde_json::json!({ "title": "Renamed anonymously" });
        let response = app
            .oneshot(
                Request::patch(format!("/api/meetings/{code}"))
                    .header("content-type", "application/json")
                    .body(Body::from(patch.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "anonymous meetings stay editable by anyone with the code"
        );
    }

    #[tokio::test]
    async fn meetings_export_as_ics() {
        let state = test_state().await;
        let app = router(state.clone());
        let (status, body) = create_meeting_via(app, "Standup").await;
        assert_eq!(status, StatusCode::CREATED, "body: {body}");
        let created: MeetingResponse = serde_json::from_str(&body).unwrap();

        let response = router(state)
            .oneshot(
                Request::get(format!("/api/meetings/{}/ics", created.meeting.code))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/calendar; charset=utf-8"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let ics = String::from_utf8_lossy(&bytes);
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"), "got: {ics}");
        assert!(ics.contains("SUMMARY:Standup"));
        assert!(
            ics.contains(&format!("/m/{}", created.meeting.code)),
            "join link must be in the calendar: {ics}"
        );
    }

    #[tokio::test]
    async fn metrics_scrape_is_prometheus_text() {
        let response = router(test_state().await)
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap()
            .to_owned();
        assert!(
            content_type.starts_with("text/plain"),
            "got: {content_type}"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("# TYPE huddle_rooms gauge"));
        assert!(text.contains("huddle_rooms 0"));
    }

    #[tokio::test]
    async fn metrics_token_is_enforced_when_configured() {
        let config = Arc::new(crate::config::Config {
            media_port: 0,
            public_ip: Some(std::net::IpAddr::from([127, 0, 0, 1])),
            stats_token: Some("secret-token".into()),
            ..crate::config::Config::default()
        });
        let (engine, _media_addr) = crate::engine::spawn(&config).unwrap();
        let state = AppState::new(config, crate::db::test_pool().await, engine);
        let app = router(state);

        let denied = app
            .clone()
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let wrong = app
            .clone()
            .oneshot(
                Request::get("/metrics")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let allowed = app
            .oneshot(
                Request::get("/metrics")
                    .header("authorization", "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
    }
}
