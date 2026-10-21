//! Tray wiring for the shell. The *decisions* (which items exist, labels,
//! enabled state) come from `huddle_desktop_core::tray::tray_menu`; this
//! module only turns that pure model into real Tauri menu objects and
//! routes events to the commands. Rebuilt from the model after every
//! settings change so the tray always reflects the configured state.

use std::path::PathBuf;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use huddle_desktop_core as desk;
use huddle_desktop_core::tray::TrayAction;

use crate::commands;

/// Stable tray id; `rebuild` looks the tray up by it after setup.
pub const TRAY_ID: &str = "huddle-tray";

fn action_id(action: TrayAction) -> &'static str {
    match action {
        TrayAction::Open => "open",
        TrayAction::JoinNext => "join-next",
        TrayAction::Settings => "settings",
        TrayAction::Quit => "quit",
    }
}

/// Show and focus the main window (used by tray interactions).
fn show_main(app: &AppHandle) {
    if let Some(window) = app.webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// (Re)build the tray icon + menu from the current settings.
///
/// Called once from `setup` (the tray does not exist yet) and again after
/// every settings save (menu refresh via `set_menu`, keeping the event
/// handlers installed at build time).
pub(crate) fn rebuild(app: &AppHandle) -> tauri::Result<()> {
    let path = app.state::<PathBuf>();
    let settings = desk::Settings::load(path.inner());
    let specs = desk::tray::tray_menu(&settings);

    let mut items: Vec<MenuItem<Wry>> = Vec::with_capacity(specs.len());
    for spec in &specs {
        items.push(MenuItem::with_id(
            app,
            action_id(spec.action),
            &spec.label,
            spec.enabled,
            None::<&str>,
        )?);
    }
    let menu = Menu::with_items(app, &items)?;

    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu))
    } else {
        let mut builder = TrayIconBuilder::with_id(TRAY_ID)
            .menu(&menu)
            .show_menu_on_left_click(true)
            .on_menu_event(handle_menu_event)
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    show_main(tray.app_handle());
                }
            });
        if let Some(icon) = app.default_window_icon() {
            builder = builder.icon(icon.clone());
        }
        builder.build(app)
    }
}

fn handle_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        "open" => {
            show_main(app);
            let path = app.state::<PathBuf>();
            let settings = desk::Settings::load(path.inner());
            match settings.server() {
                Some(origin) => {
                    if let Err(e) = commands::open_url(app, &origin.dashboard_url()) {
                        eprintln!("tray: could not open the dashboard: {e}");
                    }
                }
                None => {
                    if let Err(e) = commands::go_home(app) {
                        eprintln!("tray: could not open the home page: {e}");
                    }
                }
            }
        }
        "join-next" => {
            // Menu event handlers cannot await; the shared implementation
            // runs on Tauri's async runtime like the command does.
            let handle = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = commands::join_next_meeting(handle).await {
                    eprintln!("tray: join next meeting failed: {e}");
                }
            });
        }
        "settings" => {
            show_main(app);
            if let Err(e) = commands::go_home(app) {
                eprintln!("tray: could not open the home page: {e}");
            }
        }
        "quit" => app.exit(0),
        _ => {}
    }
}
