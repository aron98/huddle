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
//!
//! `public_ip` may be omitted: the server then advertises its detected
//! primary outbound address at startup. That is what local development
//! wants; production deployments should set it explicitly.

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
    /// Public IP the SFU advertises in ICE candidates.
    ///
    /// `None` (the default) auto-detects the machine's primary outbound
    /// address at startup, which is right for local development. Production
    /// deployments should set it explicitly: a VPS's public address, or
    /// whatever address browsers can actually reach the media socket on.
    pub public_ip: Option<IpAddr>,
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
    /// How many participants may be in one meeting at once.
    pub max_participants: usize,
    /// Minutes a magic sign-in link stays valid.
    pub login_token_ttl_min: u32,
    /// Secret key for signing organizer session cookies. Unset (the
    /// default) generates a random key at startup with a warning —
    /// fine for trying huddle out, but sessions then do not survive
    /// restarts. Production deployments should set it.
    pub session_secret: Option<String>,
    /// Minutes an organizer session cookie stays valid.
    pub session_ttl_min: u32,
    /// SMTP relay for magic-link emails. `None` (the default) logs the
    /// link instead of sending it — fine for development and small
    /// self-hosted setups, and it keeps the single binary dependency-free.
    pub smtp: Option<SmtpConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

/// One SMTP relay, as configured in `huddle.toml`:
///
/// ```toml
/// [smtp]
/// host = "smtp.example.com"
/// port = 587
/// username = "huddle@example.com"
/// password = "…"
/// from = "huddle@example.com"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([0, 0, 0, 0], 3000)),
            public_ip: None,
            media_port: 40_000,
            data_dir: PathBuf::from("./data"),
            base_url: "http://localhost:3000".into(),
            web_dir: None,
            join_window_lead_min: 5,
            join_window_grace_min: 15,
            ice_servers: Vec::new(),
            max_participants: 8,
            login_token_ttl_min: 15,
            session_secret: None,
            session_ttl_min: 120,
            smtp: None,
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

    /// The IP browsers should be told to send media to.
    ///
    /// An explicitly configured `public_ip` wins; otherwise the machine's
    /// primary outbound address is auto-detected. The loopback fallback at
    /// the end of the chain only works when clients also send from
    /// loopback — notably, macOS Chrome does not (it sources loopbound
    /// traffic from the default interface, which fails ICE's response
    /// source validation).
    pub fn effective_public_ip(&self) -> IpAddr {
        self.effective_public_ip_with(detect_public_ip())
    }

    /// Testable core of [`Config::effective_public_ip`]: the detection
    /// result is injected so the fallback chain can be exercised without
    /// touching the network.
    fn effective_public_ip_with(&self, detected: Option<IpAddr>) -> IpAddr {
        self.public_ip.or(detected).unwrap_or_else(|| {
            tracing::warn!(
                "could not auto-detect an outbound address; advertising \
                 loopback (set HUDDLE_PUBLIC_IP if clients cannot connect)"
            );
            IpAddr::from([127, 0, 0, 1])
        })
    }

    /// Join window for a meeting: `[starts_at - lead, ends_at + grace]`.
    pub fn join_window(
        &self,
        starts_at: time::OffsetDateTime,
        ends_at: time::OffsetDateTime,
    ) -> (time::OffsetDateTime, time::OffsetDateTime) {
        let lead = time::Duration::minutes(i64::from(self.join_window_lead_min));
        let grace = time::Duration::minutes(i64::from(self.join_window_grace_min));
        (starts_at - lead, ends_at + grace)
    }
}

/// Detect the machine's primary outbound IP by asking the kernel which
/// source address it would pick for a public destination. Connecting a
/// UDP socket never sends a packet — it only installs a route.
fn detect_public_ip() -> Option<IpAddr> {
    for (bind, probe) in [
        ("0.0.0.0:0", "8.8.8.8:80"),
        ("[::]:0", "[2001:4860:4860::8888]:80"),
    ] {
        let Ok(socket) = std::net::UdpSocket::bind(bind) else {
            continue;
        };
        if socket.connect(probe).is_err() {
            continue;
        }
        if let Ok(local) = socket.local_addr() {
            let ip = local.ip();
            if !(ip.is_loopback() || ip.is_unspecified()) {
                return Some(ip);
            }
        }
    }
    None
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
        assert_eq!(config.public_ip, None);
        assert_eq!(config.media_port, 40_000);
        assert_eq!(config.join_window_lead_min, 5);
        assert_eq!(config.join_window_grace_min, 15);
        assert_eq!(config.max_participants, 8);
        assert_eq!(config.web_dir, None);
        assert_eq!(config.login_token_ttl_min, 15);
        assert_eq!(config.session_secret, None);
        assert_eq!(config.session_ttl_min, 120);
        assert_eq!(config.smtp, None);
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

    #[test]
    fn join_window_math() {
        let config = Config::default();
        let start = time::OffsetDateTime::UNIX_EPOCH;
        let end = start + time::Duration::hours(1);
        let (opens, closes) = config.join_window(start, end);
        assert_eq!(opens, start - time::Duration::minutes(5));
        assert_eq!(closes, end + time::Duration::minutes(15));
    }

    #[test]
    fn explicit_public_ip_wins_over_detection() {
        let config: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(r#"public_ip = "192.0.2.10""#))
            .extract()
            .unwrap();
        let explicit: IpAddr = "192.0.2.10".parse().unwrap();
        assert_eq!(config.effective_public_ip_with(None), explicit);
        let detected: IpAddr = "198.51.100.7".parse().unwrap();
        assert_eq!(config.effective_public_ip_with(Some(detected)), explicit);
    }

    #[test]
    fn detected_public_ip_is_used_when_unconfigured() {
        let config = Config::default();
        let detected: IpAddr = "203.0.113.1".parse().unwrap();
        assert_eq!(config.effective_public_ip_with(Some(detected)), detected);
    }

    #[test]
    fn public_ip_falls_back_to_loopback_when_undetectable() {
        let config = Config::default();
        assert_eq!(
            config.effective_public_ip_with(None),
            IpAddr::from([127, 0, 0, 1])
        );
    }

    #[test]
    fn smtp_section_parses() {
        let config: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(
                r#"
                [smtp]
                host = "smtp.example.com"
                port = 587
                username = "huddle@example.com"
                "#,
            ))
            .extract()
            .unwrap();
        let smtp = config.smtp.expect("smtp section");
        assert_eq!(smtp.host, "smtp.example.com");
        assert_eq!(smtp.port, 587);
        assert_eq!(smtp.username.as_deref(), Some("huddle@example.com"));
        assert_eq!(smtp.password, None);
        assert_eq!(smtp.from, None);
        // Untouched defaults survive alongside the section.
        assert_eq!(config.login_token_ttl_min, 15);
    }
}
