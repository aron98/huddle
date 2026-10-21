//! The commands the shell home page invokes via `window.__TAURI__.core.invoke`
//! and the tray handlers call directly.
//!
//! Every piece of decision-making lives in `huddle-desktop-core`; these
//! wrappers only move data between Tauri, the settings file, and the
//! network, so nothing here needs a GUI to be reviewed for correctness.

use std::path::PathBuf;

use serde::Serialize;
use tauri::{AppHandle, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_http::reqwest;

use huddle_desktop_core as desk;

/// Outcome of the tray's "Join next meeting". The tag/type field keeps the
/// JSON shape self-describing for the UI layer.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NextMeetingOutcome {
    /// The next upcoming meeting was found; its join page is open.
    Joined { url: String },
    /// No meeting could be joined through the shell's own API access, so
    /// the dashboard was opened instead (see `reason`; today the common
    /// case is the 401 described in ADR 014).
    Dashboard { url: String, reason: String },
}

impl NextMeetingOutcome {
    fn url(&self) -> &str {
        match self {
            Self::Joined { url } | Self::Dashboard { url, .. } => url,
        }
    }
}

/// Result of the settings page's reachability probe.
#[derive(Debug, Serialize)]
pub struct ServerCheck {
    pub ok: bool,
    pub detail: String,
}

/// Show `url` in the main window. The URL is JSON-encoded into a JS string
/// literal so quoting can never break — `serde_json::to_string` on a
/// `&str` is exactly a quoted-and-escaped literal. `location.replace`
/// keeps the back stack clean when the shell swaps servers.
///
/// `open_url` targets remote (server) pages; for the bundled home page use
/// [`go_home`], which needs Tauri's own URL routing, not JS.
pub(crate) fn open_url(app: &AppHandle, url: &str) -> Result<(), String> {
    match app.webview_window("main") {
        Some(window) => {
            let literal = serde_json::to_string(url).unwrap_or_else(|_| format!("{url:?}"));
            window
                .eval(&format!("window.location.replace({literal});"))
                .map_err(|e| e.to_string())
        }
        None => {
            // The window was never created (only possible before startup
            // completes); recreate it at the target.
            let parsed = tauri::Url::parse(url).map_err(|e| e.to_string())?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(parsed))
                .title("huddle")
                .inner_size(1200.0, 800.0)
                .build()
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    }
}

/// Navigate the main window back to the bundled shell home page.
pub(crate) fn go_home(app: &AppHandle) -> Result<(), String> {
    let window = app
        .webview_window("main")
        .ok_or_else(|| "the main window does not exist".to_owned())?;
    window
        .navigate(WebviewUrl::App("index.html".into()))
        .map_err(|e| e.to_string())
}

/// Load the persisted settings for the home page.
#[tauri::command]
pub fn load_settings(path: State<'_, PathBuf>) -> desk::Settings {
    desk::Settings::load(path.inner())
}

/// Validate + persist the server address, rebuild the tray, and connect
/// the window to the server's dashboard. Returns the normalized origin
/// so the page can update its state even if navigation is slow.
#[tauri::command]
pub fn save_and_connect(
    app: AppHandle,
    path: State<'_, PathBuf>,
    server_url: String,
) -> Result<String, String> {
    let origin = desk::ServerOrigin::parse(&server_url).map_err(|e| e.to_string())?;
    let mut settings = desk::Settings::load(path.inner());
    settings.server_url = Some(origin.as_str().to_owned());
    settings
        .save(path.inner())
        .map_err(|e| format!("could not save settings: {e}"))?;
    crate::tray::rebuild(&app).map_err(|e| format!("could not rebuild the tray: {e}"))?;
    open_url(&app, &origin.dashboard_url())?;
    Ok(origin.as_str().to_owned())
}

/// Probe `{server}/api/health` without saving anything. Network problems
/// are a *result*, not a command error — the settings form renders the
/// detail text next to the field.
#[tauri::command]
pub async fn check_server(server_url: String) -> Result<ServerCheck, String> {
    let origin = desk::ServerOrigin::parse(&server_url).map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .timeout(desk::meetings::API_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    match client.get(desk::health_endpoint(&origin)).send().await {
        Ok(resp) => {
            let status = resp.status();
            Ok(ServerCheck {
                ok: status.is_success(),
                detail: format!("the server answered HTTP {}", status.as_u16()),
            })
        }
        Err(e) => Ok(ServerCheck {
            ok: false,
            detail: format!("could not reach {origin}: {e}"),
        }),
    }
}

/// Join from a full meeting link or a bare code. A link that points at a
/// different server than the configured one switches the remembered
/// server (the session cookie jar is per-origin, so following the user's
/// actual link is the least surprising behavior — ADR 014).
#[tauri::command]
pub fn join_by_link(
    app: AppHandle,
    path: State<'_, PathBuf>,
    input: String,
) -> Result<String, String> {
    let mut settings = desk::Settings::load(path.inner());
    let target =
        desk::parse_join_input(&input, settings.server().as_ref()).map_err(|e| e.to_string())?;
    if settings.server().as_ref() != Some(&target.origin) {
        settings.server_url = Some(target.origin.as_str().to_owned());
        settings
            .save(path.inner())
            .map_err(|e| format!("could not save settings: {e}"))?;
        crate::tray::rebuild(&app).map_err(|e| format!("could not rebuild the tray: {e}"))?;
    }
    let url = desk::build_join_url(&target.origin, &target.code);
    open_url(&app, &url)?;
    Ok(url)
}

/// Shared by the `join_next_meeting` command and the tray item: fetch the
/// upcoming meetings and open the next join page, or degrade to the
/// dashboard with the reason why.
///
/// The honest degradation (ADR 014): the shell's HTTP client carries no
/// session credentials — the magic-link cookie lives in the webview's
/// per-origin cookie jar, which the shell cannot read — so
/// `/api/meetings/mine` answers 401 today and the dashboard's own
/// logged-in meeting list is the working substitute. The pure selection
/// logic in `desk::meetings::next_join_target` is ready for a future
/// non-browser token.
async fn join_next_impl(app: AppHandle) -> Result<NextMeetingOutcome, String> {
    let path = app.state::<PathBuf>();
    let settings = desk::Settings::load(path.inner());
    let Some(origin) = settings.server() else {
        return Err("no server configured yet — open Settings first".to_owned());
    };
    let client = reqwest::Client::builder()
        .timeout(desk::meetings::API_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let outcome = match client.get(desk::mine_endpoint(&origin)).send().await {
        Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
            let body = resp.text().await.map_err(|e| e.to_string())?;
            let rows = desk::meetings::parse_mine_response(&body).map_err(|e| e.to_string())?;
            let now = time::OffsetDateTime::now_utc();
            match desk::meetings::next_join_target(&rows, now) {
                Some((code, _starts_at)) => NextMeetingOutcome::Joined {
                    url: desk::build_join_url(&origin, &code),
                },
                None => NextMeetingOutcome::Dashboard {
                    url: origin.dashboard_url(),
                    reason: "no upcoming meetings found".to_owned(),
                },
            }
        }
        Ok(resp) => NextMeetingOutcome::Dashboard {
            url: origin.dashboard_url(),
            reason: format!(
                "the server answered HTTP {} — the shell has no session of its \
                 own (ADR 014), so the dashboard was opened for the meeting list",
                resp.status().as_u16()
            ),
        },
        Err(e) => NextMeetingOutcome::Dashboard {
            url: origin.dashboard_url(),
            reason: format!("could not reach {origin}: {e}"),
        },
    };
    open_url(&app, outcome.url())?;
    Ok(outcome)
}

/// Tray + home page: open the next upcoming meeting, or the dashboard.
#[tauri::command]
pub async fn join_next_meeting(app: AppHandle) -> Result<NextMeetingOutcome, String> {
    join_next_impl(app).await
}
