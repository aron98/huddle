//! Runtime configuration: a single TOML file plus `HUDDLE_*` env overrides.
//!
//! ```toml
//! # huddle.toml
//! bind = "0.0.0.0:3000"
//! public_ip = "203.0.113.10"
//! ```
//!
//! Every field can be overridden: `HUDDLE_BIND="0.0.0.0:4000"`. The file path
//! itself comes from `HUDDLE_CONFIG` (default: `./huddle.toml`, optional).

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// HTTPS/HTTP bind address for the web + signaling server.
    pub bind: SocketAddr,
    /// Public IP the SFU advertises in ICE candidates. On a VPS this must be
    /// the machine's public address, not a loopback.
    pub public_ip: IpAddr,
    /// Single UDP port the SFU's ICE/media socket binds to.
    pub media_port: u16,
    /// Where the SQLite database and other runtime state live.
    pub data_dir: PathBuf,
    /// Base URL used when the server builds absolute links (ICS, magic links).
    pub base_url: String,
    /// Optional filesystem override for web assets (dev: point at web/dist
    /// while `trunk watch` rebuilds). Falls back to the embedded bundle.
    pub web_dir: Option<PathBuf>,
    /// Minutes before `starts_at` that joining opens.
    pub join_window_lead_min: u32,
    /// Minutes after `ends_at` that joining stays open.
    pub join_window_grace_min: u32,
    /// IceServers advertised to clients in addition to the SFU's own
    /// candidates (e.g. coturn).
    pub ice_servers: Vec<IceServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 3000)),
            public_ip: IpAddr::from([127, 0, 0, 1]),
            media_port: 40_000,
            data_dir: PathBuf::from("./data"),
            base_url: "http://localhost:3000".into(),
            web_dir: None,
            join_window_lead_min: 5,
            join_window_grace_min: 15,
            ice_servers: Vec::new(),
        }
    }
}

impl Config {
    /// Load configuration: defaults ← config file ← `HUDDLE_*` env.
    ///
    /// The file is optional unless `HUDDLE_CONFIG` points at one explicitly,
    /// in which case it must exist (a typo'd path should fail loudly).
    pub fn load() -> anyhow::Result<Self> {
        let mut figment = Figment::from(Serialized::defaults(Self::default()));

        let explicit = std::env::var_os("HUDDLE_CONFIG");
        let default_path = std::path::Path::new("huddle.toml");
        let path: PathBuf = match &explicit {
            Some(p) => {
                let path = PathBuf::from(p);
                if !path.exists() {
                    anyhow::bail!("HUDDLE_CONFIG points at a missing file: {}", path.display());
                }
                path
            }
            None if default_path.exists() => default_path.to_path_buf(),
            None => PathBuf::from("huddle.toml"),
        };
        if path.exists() {
            figment = figment.merge(Toml::file(path));
        }

        let config: Self = figment.merge(Env::prefixed("HUDDLE_")).extract()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply() {
        let config: Config = Figment::from(Serialized::defaults(Config::default()))
            .extract()
            .unwrap();
        assert_eq!(config.bind.port(), 3000);
        assert_eq!(config.media_port, 40_000);
        assert_eq!(config.join_window_lead_min, 5);
        assert_eq!(config.join_window_grace_min, 15);
        assert_eq!(config.web_dir, None);
    }

    #[test]
    fn toml_overrides_defaults() {
        let config: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(r#"bind = "0.0.0.0:9999""#))
            .extract()
            .unwrap();
        assert_eq!(config.bind.port(), 9999);
        assert_eq!(config.join_window_grace_min, 15); // untouched default
    }
}
