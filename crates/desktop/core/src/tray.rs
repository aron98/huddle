//! The tray/menu model, as pure data.
//!
//! The app layer turns [`tray_menu`] output into real `tauri::menu` items;
//! keeping the decision here means "what does the tray offer, given the
//! current settings" is a testable function instead of UI wiring. The
//! tray is rebuilt from this model after every settings save.

use crate::settings::Settings;

/// What a tray interaction asks the shell to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Open (or bring to front) the main window on the server's page —
    /// the dashboard, or the shell home page when no server is configured.
    Open,
    /// Fetch the upcoming-meetings list and open the next join page
    /// (falls back to the dashboard when unauthenticated — ADR 014).
    JoinNext,
    /// The shell home page: server field plus the join-by-link/code box.
    Settings,
    /// Exit the app entirely.
    Quit,
}

/// One tray menu item: the action, its label, and whether it is
/// interactive in the current state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItemSpec {
    pub action: TrayAction,
    pub label: String,
    pub enabled: bool,
}

/// The tray menu for the current settings.
///
/// Unconfigured shell: only Settings and Quit do anything. Configured:
/// Open lands on the server dashboard; JoinNext stays enabled even though
/// its fetch may come back 401 — the command's documented degradation path
/// is opening the dashboard, which is still useful.
pub fn tray_menu(settings: &Settings) -> Vec<MenuItemSpec> {
    let configured = settings.server().is_some();
    vec![
        MenuItemSpec {
            action: TrayAction::Open,
            label: if configured {
                "Open huddle".to_owned()
            } else {
                "Open (no server configured)".to_owned()
            },
            enabled: true,
        },
        MenuItemSpec {
            action: TrayAction::JoinNext,
            label: "Join next meeting".to_owned(),
            enabled: configured,
        },
        MenuItemSpec {
            action: TrayAction::Settings,
            label: "Settings & join by code…".to_owned(),
            enabled: true,
        },
        MenuItemSpec {
            action: TrayAction::Quit,
            label: "Quit".to_owned(),
            enabled: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unconfigured_shell_disables_server_actions() {
        let menu = tray_menu(&Settings::default());
        assert_eq!(menu.len(), 4);
        let open = &menu[0];
        assert_eq!(open.action, TrayAction::Open);
        assert!(open.label.contains("no server"));
        assert!(open.enabled);
        assert!(!menu[1].enabled, "JoinNext needs a configured server");
        assert!(menu[2].enabled);
        assert!(menu[3].enabled);
    }

    #[test]
    fn configured_shell_enables_everything() {
        let settings = Settings {
            server_url: Some("https://meet.example.com".to_owned()),
        };
        let menu = tray_menu(&settings);
        assert!(menu.iter().all(|item| item.enabled));
        assert_eq!(menu[0].label, "Open huddle");
    }

    #[test]
    fn actions_cover_a_stable_order() {
        // The app matches menu items by action; the order is part of the
        // contract the tray wiring relies on.
        let actions: Vec<_> = tray_menu(&Settings::default())
            .into_iter()
            .map(|item| item.action)
            .collect();
        assert_eq!(
            actions,
            vec![
                TrayAction::Open,
                TrayAction::JoinNext,
                TrayAction::Settings,
                TrayAction::Quit,
            ]
        );
    }
}
