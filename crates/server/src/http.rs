//! HTTP router: API routes plus the embedded web UI.

use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde_json::json;
use time::OffsetDateTime;

use huddle_protocol::{
    ApiError, CreateMeetingRequest, ErrorCode, MeetingResponse, UpdateMeetingRequest,
};

use crate::meetings::{self, MeetingError};
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
        .route(
            "/api/meetings/{code}",
            get(get_meeting)
                .patch(update_meeting)
                .delete(cancel_meeting),
        )
        .route("/ws", get(session::ws))
        .fallback(fallback)
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn create_meeting(
    state: State<AppState>,
    Json(req): Json<CreateMeetingRequest>,
) -> Result<(StatusCode, Json<MeetingResponse>), ApiErr> {
    let meeting = meetings::create(state.pool(), &req, OffsetDateTime::now_utc())
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
    Path(code): Path<String>,
    Json(req): Json<UpdateMeetingRequest>,
) -> Result<Json<MeetingResponse>, ApiErr> {
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
    Path(code): Path<String>,
) -> Result<Json<MeetingResponse>, ApiErr> {
    let meeting = meetings::set_cancelled(state.pool(), &code, true)
        .await
        .map_err(ApiErr::internal)?
        .ok_or_else(ApiErr::not_found)?;
    Ok(Json(MeetingResponse {
        joinability: meetings::joinability(&meeting, state.config(), OffsetDateTime::now_utc()),
        meeting: meeting.into(),
    }))
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
}
