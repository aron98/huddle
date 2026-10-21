//! The huddle desktop shell (issue #18, ADR 014).
//!
//! The window shows the shell's own home page (bundled `../ui`) until a
//! server is configured; after that it lives on the huddle server's UI,
//! which the server itself serves from its single binary (ADR 004) — the
//! shell never rebuilds or rehosts the web client. The tray mirrors
//! [`huddle_desktop_core::tray`]'s pure menu model.

// No extra console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod tray;

use tauri::{Manager, WindowEvent};

fn main() -> tauri::Result<()> {
    tauri::Builder::default()
        .plugin(tauri_plugin_http::init())
        .invoke_handler(tauri::generate_handler![
            commands::load_settings,
            commands::save_and_connect,
            commands::check_server,
            commands::join_by_link,
            commands::join_next_meeting,
        ])
        .setup(|app| {
            // The settings file lives in the per-platform app config dir
            // (e.g. ~/.config/app.huddle.desktop); core owns the format,
            // this owns the location. Managed as a PathBuf so every
            // command gets it via State.
            let dir = app.path().app_config_dir()?;
            app.manage(dir.join(huddle_desktop_core::settings::SETTINGS_FILE));
            tray::rebuild(app.handle())?;
            Ok(())
        })
        // Closing the window hides it: a shell with a tray keeps running
        // so "Join next meeting" stays one click away. The tray's Quit
        // item is the real exit.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
}
