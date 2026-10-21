//! Storage for the CLI's long-lived credentials (issue #9).
//!
//! The refresh token is the crown jewel: it can mint access tokens for
//! weeks. It lives in a single JSON file with owner-only permissions,
//! at the path given by `--token-file`, or `$HUDDLE_CLI_TOKEN_FILE`,
//! or `~/.config/huddle/cli.json`. An OS keychain is the right eventual
//! home; that is deliberately deferred (ADR 013) — the explicit-path
//! functions here are the seam a future backend plugs into.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// The stored login: which server, and the refresh token for it. The
/// access token is cached alongside so repeated commands skip one
/// network round-trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredLogin {
    pub server: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// Unix seconds when the cached access token expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_expires_at: Option<i64>,
}

/// Resolve the token file: explicit flag, env override, then default.
pub fn token_file(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Ok(path) = std::env::var("HUDDLE_CLI_TOKEN_FILE") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    Path::new(&home)
        .join(".config")
        .join("huddle")
        .join("cli.json")
}

/// Read the stored login, if any.
pub fn load_from(path: &Path) -> Option<StoredLogin> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist the login with owner-only permissions.
pub fn save_to(path: &Path, login: &StoredLogin) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_string_pretty(login).map_err(std::io::Error::other)?;
    fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Forget the stored login (the file is already dead weight once the
/// server-side refresh token is revoked).
pub fn clear_at(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("huddle-cli-store-{name}-{}", std::process::id()))
    }

    #[test]
    fn saved_logins_round_trip_with_owner_only_permissions() {
        let path = temp_path("round-trip");
        let login = StoredLogin {
            server: "https://huddle.example.com".to_owned(),
            refresh_token: "refresh".to_owned(),
            access_token: Some("access".to_owned()),
            access_expires_at: Some(1_700_000_000),
        };
        save_to(&path, &login).expect("save");
        let loaded = load_from(&path).expect("load");
        assert_eq!(loaded.server, login.server);
        assert_eq!(loaded.refresh_token, login.refresh_token);
        assert_eq!(loaded.access_token.as_deref(), Some("access"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        clear_at(&path);
        assert!(load_from(&path).is_none());
    }

    #[test]
    fn missing_or_broken_files_read_as_no_login() {
        assert!(load_from(Path::new("/definitely/not/here/cli.json")).is_none());
        let path = temp_path("broken");
        fs::write(&path, "not json").expect("write");
        assert!(load_from(&path).is_none());
        clear_at(&path);
    }
}
