//! Shell settings, persisted as one small JSON file.
//!
//! The store is deliberately plain — `serde` + `std::fs`, no Tauri plugin —
//! so it is unit-testable on a headless machine. The app passes a path from
//! Tauri's `app_config_dir()`; every function here takes that path as an
//! argument instead of knowing where it lives.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Persisted shell configuration. Written by the settings/home page via the
/// `save_and_connect` command; read by every command that needs the server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// The normalized server origin ([`crate::server::ServerOrigin`] string).
    /// `None` until the first successful connect — the tray and the home
    /// page treat that as "shell not configured yet".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
}

impl Settings {
    /// Load settings from `path`.
    ///
    /// A missing file is a normal first run, and a corrupt file must never
    /// brick the shell: both yield default settings (the caller may want to
    /// log the parse failure). Never panics.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save settings to `path`, atomically.
    ///
    /// Writes to a sibling temp file and renames over the target so a crash
    /// mid-write can never leave a truncated JSON behind (the file is tiny;
    /// the temp-and-rename costs nothing).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    /// The configured server, re-validated on load.
    ///
    /// A stored value that no longer parses (hand-edited file, partial
    /// write from an older shell) is treated as unset — same first-run
    /// behavior as no file at all.
    pub fn server(&self) -> Option<crate::server::ServerOrigin> {
        self.server_url
            .as_deref()
            .and_then(|url| crate::server::ServerOrigin::parse(url).ok())
    }
}

/// The conventional settings file name inside the app config directory.
pub const SETTINGS_FILE: &str = "settings.json";

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp path per test — the VM's `/tmp` is shared, and parallel
    /// test processes would race on a fixed name.
    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "huddle-desktop-core-{}-{name}.json",
            std::process::id()
        ));
        p
    }

    #[test]
    fn missing_file_is_default() {
        let p = temp_path("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(Settings::load(&p), Settings::default());
    }

    #[test]
    fn round_trips_through_disk() {
        let p = temp_path("roundtrip");
        let original = Settings {
            server_url: Some("https://meet.example.com".to_owned()),
        };
        original.save(&p).unwrap();
        assert_eq!(Settings::load(&p), original);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn saves_snake_case_json() {
        let p = temp_path("shape");
        Settings {
            server_url: Some("https://meet.example.com".to_owned()),
        }
        .save(&p)
        .unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("\"server_url\""),
            "snake_case on disk: {text}"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unconfigured_settings_omit_the_field() {
        let text = serde_json::to_string(&Settings::default()).unwrap();
        assert_eq!(text, "{}");
    }

    #[test]
    fn corrupt_file_yields_defaults() {
        let p = temp_path("corrupt");
        std::fs::write(&p, "{not json at all").unwrap();
        assert_eq!(Settings::load(&p), Settings::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn legacy_payload_without_field_loads() {
        let p = temp_path("legacy");
        std::fs::write(&p, r#"{"something_else": true}"#).unwrap();
        assert_eq!(Settings::load(&p), Settings::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_creates_parent_directories() {
        let mut p = temp_path("nested");
        p.pop();
        p.push("created-by-test/nested/settings.json");
        Settings {
            server_url: Some("http://localhost:3000".to_owned()),
        }
        .save(&p)
        .unwrap();
        assert!(p.exists());
        std::fs::remove_dir_all(p.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn server_revalidates_the_stored_value() {
        let good = Settings {
            server_url: Some("https://meet.example.com".to_owned()),
        };
        assert_eq!(good.server().unwrap().as_str(), "https://meet.example.com");

        // A value that no longer parses is treated as unset.
        let bad = Settings {
            server_url: Some("meet.example.com".to_owned()),
        };
        assert!(bad.server().is_none());

        let unconfigured = Settings::default();
        assert!(unconfigured.server().is_none());
    }
}
