#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! Pure logic for the huddle desktop shell (issue #18, ADR 014).
//!
//! Everything here is platform-independent and free of Tauri/system
//! dependencies, so the whole crate builds and its tests run on a bare
//! headless machine (this repo's CI runs `cargo test -p huddle-desktop-core`
//! inside `crates/desktop`). The `app` member wires these types and
//! functions into the Tauri shell; keeping the logic here is what makes
//! it verifiable without a GUI.
//!
//! The shell's serving model (ADR 014): the web client is strictly
//! same-origin (relative-path `fetch` in `crates/web/src/api.rs`, the
//! signaling WebSocket built from `window.location.host` in
//! `crates/web/src/call.rs`), so the shell does NOT bundle the web
//! client. It points the webview at the huddle server itself, which
//! serves the UI from its single binary (ADR 004). Zero changes to
//! `crates/web`, no CORS anywhere, and the session cookie lives in the
//! webview's per-origin cookie jar — "remembers auth per server".

pub mod capabilities;
pub mod links;
pub mod meetings;
pub mod server;
pub mod settings;
pub mod tray;

pub use capabilities::{Platform, PlatformCapability, Support, platform_capability};
pub use links::{JoinTarget, build_join_url, parse_join_input};
pub use meetings::{
    MeetingRow, health_endpoint, mine_endpoint, next_join_target, parse_mine_response,
};
pub use server::{ServerOrigin, ServerUrlError};
pub use settings::Settings;
pub use tray::{MenuItemSpec, TrayAction, tray_menu};
