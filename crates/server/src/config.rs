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
    /// SMTP relay for email (`[smtp]`). A section on its own still
    /// selects the SMTP transport, exactly as before issue #35; the
    /// general selection mechanism is [`Config::mail`].
    pub smtp: Option<SmtpConfig>,
    /// Mail delivery selection (issue #35): the transport plus its
    /// provider-specific settings. Unset (the default) auto-detects —
    /// `[smtp]` alone keeps delivering through the relay, nothing
    /// configured logs the email instead of sending it, so evaluating
    /// huddle never requires an email account. See [`MailConfig`].
    pub mail: MailConfig,
    /// OIDC identity provider for SSO (issue #11). `None` (the default)
    /// keeps magic links as the organizer sign-in flow; when set, SSO
    /// replaces them. See [`OidcConfig`].
    pub oidc: Option<OidcConfig>,
    /// Optional bearer token protecting `GET /metrics`. `None` (the
    /// default) leaves the endpoint open, matching Prometheus's
    /// pull model: deployments are expected to gate it at the network
    /// layer (firewall, reverse proxy, or a Prometheus/Grafana sidecar
    /// that is the only thing with network access). Set it to require
    /// `Authorization: Bearer <token>` on every scrape.
    pub stats_token: Option<String>,
    /// Per-IP rate limit (events per minute) on magic-link requests and
    /// new websocket connections. `30` (the default) is generous for
    /// humans and throttles credential-stuffing and connection floods;
    /// `0` disables the limiter entirely.
    pub rate_limit_per_min: usize,
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
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// Serde default for [`SmtpConfig::port`]: submission over STARTTLS.
fn default_smtp_port() -> u16 {
    587
}

/// Which transport delivers huddle's email (issue #35, ADR 006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MailProvider {
    /// Log the email at `info` level — the dependency-free default.
    Log,
    /// Send through the `[smtp]` relay (lettre STARTTLS).
    Smtp,
    /// Deliver via the Resend HTTPS API (`[mail.resend]`).
    Resend,
}

impl MailProvider {
    /// Parse a provider name, e.g. from `HUDDLE_MAIL_PROVIDER`. A typo
    /// fails loudly, naming the valid values.
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim() {
            "log" => Ok(Self::Log),
            "smtp" => Ok(Self::Smtp),
            "resend" => Ok(Self::Resend),
            other => anyhow::bail!(
                "unknown mail provider {other:?}; valid values are \"log\", \"smtp\", \"resend\""
            ),
        }
    }
}

/// Mail delivery selection, as configured in `huddle.toml`:
///
/// ```toml
/// [mail]
/// provider = "resend"        # "log" | "smtp" | "resend"
///
/// [mail.resend]
/// api_key = "re_…"
/// from = "huddle@example.com"
/// ```
///
/// `provider` may be omitted: huddle then detects the transport from
/// whichever section is present — `[mail.resend]` selects Resend, a
/// bare `[smtp]` keeps legacy deployments delivering exactly as
/// before, nothing configured logs the email. An explicit `provider`
/// always wins, and its section must exist.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MailConfig {
    /// Pins the transport when set; `None` auto-detects.
    pub provider: Option<MailProvider>,
    /// Resend API credentials; required when the provider is (or
    /// detects to) `resend`.
    pub resend: Option<ResendConfig>,
}

/// Resend delivery, as configured in `huddle.toml`:
///
/// ```toml
/// [mail.resend]
/// api_key = "re_…"
/// from = "huddle@example.com"
/// ```
///
/// Or per-field via `HUDDLE_MAIL_RESEND_API_KEY` /
/// `HUDDLE_MAIL_RESEND_FROM`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ResendConfig {
    /// Resend API key (`re_…`) from the dashboard.
    pub api_key: String,
    /// Envelope sender, e.g. `"Huddle <huddle@example.com>"`.
    pub from: String,
}

/// The transport a configuration resolves to (issue #35, ADR 006):
/// decided and validated here — pure data, no clients built — so
/// config tests can cover every combination without touching the
/// network. Startup turns it into a `mailer::Mailer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailSelection {
    /// Log emails at `info` level.
    Log,
    /// Deliver through the SMTP relay.
    Smtp(SmtpConfig),
    /// Deliver through the Resend API.
    Resend(ResendConfig),
}

/// OIDC single sign-on, as configured in `huddle.toml`:
///
/// ```toml
/// [oidc]
/// issuer_url = "https://accounts.example.com"
/// client_id = "huddle"
/// client_secret = "…"
/// ```
///
/// Or per-field via `HUDDLE_OIDC_ISSUER_URL`, `HUDDLE_OIDC_CLIENT_ID`,
/// `HUDDLE_OIDC_CLIENT_SECRET`. The redirect URI registered with the IdP
/// must be `<base_url>/api/auth/oidc/callback`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct OidcConfig {
    /// The issuer identifier; discovery lives at
    /// `<issuer_url>/.well-known/openid-configuration`.
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
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
            mail: MailConfig::default(),
            oidc: None,
            stats_token: None,
            rate_limit_per_min: 30,
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

        // Figment's flat `HUDDLE_*` env keys cannot reach a nested
        // `[oidc]` section, so the three OIDC variables are applied
        // explicitly after extraction.
        let env_oidc = (
            std::env::var_os("HUDDLE_OIDC_ISSUER_URL"),
            std::env::var_os("HUDDLE_OIDC_CLIENT_ID"),
            std::env::var_os("HUDDLE_OIDC_CLIENT_SECRET"),
        );
        let env_oidc = (
            env_oidc.0.map(|v| v.to_string_lossy().into_owned()),
            env_oidc.1.map(|v| v.to_string_lossy().into_owned()),
            env_oidc.2.map(|v| v.to_string_lossy().into_owned()),
        );
        let (issuer, client_id, secret) = env_oidc;
        let mut config = config;
        Self::apply_oidc_env_with(&mut config, issuer, client_id, secret)?;
        Self::apply_mail_env_with(&mut config, MailEnv::from_env())?;
        Ok(config)
    }

    /// Testable core of the `HUDDLE_OIDC_*` env handling: each of the
    /// three values, when set and non-empty, overrides the matching
    /// `[oidc]` TOML field. If any is set, all three must end up
    /// non-empty — a half-configured IdP would silently break sign-in.
    fn apply_oidc_env_with(
        config: &mut Self,
        issuer_url: Option<String>,
        client_id: Option<String>,
        client_secret: Option<String>,
    ) -> anyhow::Result<()> {
        let any_set = issuer_url.is_some() || client_id.is_some() || client_secret.is_some();
        if !any_set {
            return Ok(());
        }
        let mut oidc = config.oidc.clone().unwrap_or(OidcConfig {
            issuer_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
        });
        if let Some(v) = issuer_url.filter(|v| !v.trim().is_empty()) {
            oidc.issuer_url = v;
        }
        if let Some(v) = client_id.filter(|v| !v.trim().is_empty()) {
            oidc.client_id = v;
        }
        if let Some(v) = client_secret.filter(|v| !v.trim().is_empty()) {
            oidc.client_secret = v;
        }
        if oidc.issuer_url.trim().is_empty() {
            anyhow::bail!(
                "HUDDLE_OIDC_ISSUER_URL is required when any HUDDLE_OIDC_* variable is set"
            );
        }
        if oidc.client_id.trim().is_empty() {
            anyhow::bail!(
                "HUDDLE_OIDC_CLIENT_ID is required when any HUDDLE_OIDC_* variable is set"
            );
        }
        if oidc.client_secret.trim().is_empty() {
            anyhow::bail!(
                "HUDDLE_OIDC_CLIENT_SECRET is required when any HUDDLE_OIDC_* variable is set"
            );
        }
        config.oidc = Some(oidc);
        Ok(())
    }

    /// Resolve and validate the mail transport (issue #35, ADR 006).
    ///
    /// An explicit `mail.provider` wins; without one the transport is
    /// detected from whichever section is present — a bare `[smtp]`
    /// keeps existing deployments on SMTP, nothing at all logs the
    /// email (the dependency-free default), and both configured at
    /// once is an error rather than a silent pick.
    pub fn mail_selection(&self) -> anyhow::Result<MailSelection> {
        if let Some(provider) = self.mail.provider {
            return match provider {
                MailProvider::Log => Ok(MailSelection::Log),
                MailProvider::Smtp => Ok(match &self.smtp {
                    Some(smtp) => MailSelection::Smtp(smtp.clone()),
                    None => anyhow::bail!("mail.provider = \"smtp\" requires an [smtp] section"),
                }),
                MailProvider::Resend => self.selected_resend(),
            };
        }
        match (&self.mail.resend, &self.smtp) {
            (Some(_), Some(_)) => anyhow::bail!(
                "[smtp] and [mail.resend] are both configured; \
                 set mail.provider to pick one"
            ),
            (Some(_), None) => self.selected_resend(),
            (None, Some(smtp)) => Ok(MailSelection::Smtp(smtp.clone())),
            (None, None) => Ok(MailSelection::Log),
        }
    }

    /// Validate `[mail.resend]` and select it — shared by the explicit
    /// `provider = "resend"` arm and resend detection.
    fn selected_resend(&self) -> anyhow::Result<MailSelection> {
        let Some(resend) = &self.mail.resend else {
            anyhow::bail!("mail.provider = \"resend\" requires a [mail.resend] section");
        };
        if resend.api_key.trim().is_empty() {
            anyhow::bail!("[mail.resend] api_key must not be empty");
        }
        if resend.from.trim().is_empty() {
            anyhow::bail!("[mail.resend] from must not be empty");
        }
        Ok(MailSelection::Resend(resend.clone()))
    }

    /// Testable core of the `HUDDLE_MAIL_*` / `HUDDLE_SMTP_*` env
    /// handling, mirroring [`Config::apply_oidc_env_with`]: each set,
    /// non-empty value overrides the matching TOML field, merging
    /// into an existing section when there is one. A half-configured
    /// transport must fail loudly rather than silently break delivery.
    fn apply_mail_env_with(&mut self, env: MailEnv) -> anyhow::Result<()> {
        if let Some(provider) = env.provider.filter(|v| !v.trim().is_empty()) {
            self.mail.provider = Some(MailProvider::parse(&provider)?);
        }

        if env.resend_api_key.is_some() || env.resend_from.is_some() {
            let mut resend = self.mail.resend.clone().unwrap_or(ResendConfig {
                api_key: String::new(),
                from: String::new(),
            });
            if let Some(v) = env.resend_api_key.filter(|v| !v.trim().is_empty()) {
                resend.api_key = v;
            }
            if let Some(v) = env.resend_from.filter(|v| !v.trim().is_empty()) {
                resend.from = v;
            }
            if resend.api_key.trim().is_empty() {
                anyhow::bail!(
                    "HUDDLE_MAIL_RESEND_API_KEY is required when any \
                     HUDDLE_MAIL_RESEND_* variable is set"
                );
            }
            if resend.from.trim().is_empty() {
                anyhow::bail!(
                    "HUDDLE_MAIL_RESEND_FROM is required when any \
                     HUDDLE_MAIL_RESEND_* variable is set"
                );
            }
            self.mail.resend = Some(resend);
        }

        let any_smtp = env.smtp_host.is_some()
            || env.smtp_port.is_some()
            || env.smtp_username.is_some()
            || env.smtp_password.is_some()
            || env.smtp_from.is_some();
        if any_smtp {
            let mut smtp = self.smtp.clone().unwrap_or(SmtpConfig {
                host: String::new(),
                port: 587,
                username: None,
                password: None,
                from: None,
            });
            if let Some(v) = env.smtp_host.filter(|v| !v.trim().is_empty()) {
                smtp.host = v;
            }
            if let Some(v) = env.smtp_port {
                smtp.port = v.trim().parse().map_err(|_| {
                    anyhow::anyhow!("HUDDLE_SMTP_PORT must be a port number, got {v:?}")
                })?;
            }
            if let Some(v) = env.smtp_username.filter(|v| !v.trim().is_empty()) {
                smtp.username = Some(v);
            }
            if let Some(v) = env.smtp_password.filter(|v| !v.trim().is_empty()) {
                smtp.password = Some(v);
            }
            if let Some(v) = env.smtp_from.filter(|v| !v.trim().is_empty()) {
                smtp.from = Some(v);
            }
            if smtp.host.trim().is_empty() {
                anyhow::bail!(
                    "HUDDLE_SMTP_HOST is required when any HUDDLE_SMTP_* variable is set"
                );
            }
            self.smtp = Some(smtp);
        }
        Ok(())
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

/// The `HUDDLE_MAIL_*` / `HUDDLE_SMTP_*` variables, collected for
/// [`Config::apply_mail_env_with`]. Figment's flat env keys cannot
/// reach nested sections (see the OIDC note on `Config::load`), so —
/// mirroring `HUDDLE_OIDC_*` — they are applied explicitly after
/// extraction.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MailEnv {
    provider: Option<String>,
    resend_api_key: Option<String>,
    resend_from: Option<String>,
    smtp_host: Option<String>,
    smtp_port: Option<String>,
    smtp_username: Option<String>,
    smtp_password: Option<String>,
    smtp_from: Option<String>,
}

impl MailEnv {
    /// Collect the variables from the process environment.
    fn from_env() -> Self {
        let var = |key: &str| std::env::var_os(key).map(|v| v.to_string_lossy().into_owned());
        Self {
            provider: var("HUDDLE_MAIL_PROVIDER"),
            resend_api_key: var("HUDDLE_MAIL_RESEND_API_KEY"),
            resend_from: var("HUDDLE_MAIL_RESEND_FROM"),
            smtp_host: var("HUDDLE_SMTP_HOST"),
            smtp_port: var("HUDDLE_SMTP_PORT"),
            smtp_username: var("HUDDLE_SMTP_USERNAME"),
            smtp_password: var("HUDDLE_SMTP_PASSWORD"),
            smtp_from: var("HUDDLE_SMTP_FROM"),
        }
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
        assert_eq!(config.mail.provider, None);
        assert_eq!(config.mail.resend, None);
        assert_eq!(config.oidc, None);
        assert_eq!(config.stats_token, None);
        assert_eq!(config.rate_limit_per_min, 30);
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

    #[test]
    fn oidc_section_parses() {
        let config: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(
                r#"
                [oidc]
                issuer_url = "https://accounts.example.com"
                client_id = "huddle"
                client_secret = "s3cret"
                "#,
            ))
            .extract()
            .unwrap();
        let oidc = config.oidc.expect("oidc section");
        assert_eq!(oidc.issuer_url, "https://accounts.example.com");
        assert_eq!(oidc.client_id, "huddle");
        assert_eq!(oidc.client_secret, "s3cret");
        // Magic links stay the default elsewhere.
        assert_eq!(config.login_token_ttl_min, 15);
    }

    #[test]
    fn partial_oidc_env_fails() {
        let mut config = Config::default();
        let err = Config::apply_oidc_env_with(
            &mut config,
            Some("https://accounts.example.com".into()),
            None,
            None,
        );
        assert!(err.is_err(), "half-configured OIDC must not pass");
        // An empty string counts as unset and still fails validation.
        let mut config = Config::default();
        let err = Config::apply_oidc_env_with(
            &mut config,
            Some("https://accounts.example.com".into()),
            Some("".into()),
            Some("secret".into()),
        );
        assert!(err.is_err());
    }

    #[test]
    fn full_oidc_env_configures() {
        let mut config = Config::default();
        Config::apply_oidc_env_with(
            &mut config,
            Some("https://accounts.example.com".into()),
            Some("huddle".into()),
            Some("s3cret".into()),
        )
        .unwrap();
        let oidc = config.oidc.expect("oidc configured");
        assert_eq!(oidc.issuer_url, "https://accounts.example.com");
        assert_eq!(oidc.client_id, "huddle");
        assert_eq!(oidc.client_secret, "s3cret");
    }

    #[test]
    fn oidc_env_overrides_toml_per_field() {
        let config: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(
                r#"
                [oidc]
                issuer_url = "https://toml.example.com"
                client_id = "toml-client"
                client_secret = "toml-secret"
                "#,
            ))
            .extract()
            .unwrap();
        let mut config = config;
        // Env overrides client_id only; the TOML issuer and secret stay.
        Config::apply_oidc_env_with(
            &mut config,
            None,
            Some("env-client".into()),
            Some("env-secret".into()),
        )
        .unwrap();
        let oidc = config.oidc.unwrap();
        assert_eq!(oidc.issuer_url, "https://toml.example.com");
        assert_eq!(oidc.client_id, "env-client");
        assert_eq!(oidc.client_secret, "env-secret");
    }

    // --- mail transport selection (issue #35) ---------------------------

    fn config_with_toml(toml: &str) -> Config {
        Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(toml))
            .extract()
            .unwrap()
    }

    fn resend_config() -> ResendConfig {
        ResendConfig {
            api_key: "re_test_key".into(),
            from: "huddle@example.com".into(),
        }
    }

    #[test]
    fn unconfigured_mail_logs() {
        assert_eq!(
            Config::default().mail_selection().unwrap(),
            MailSelection::Log
        );
    }

    #[test]
    fn smtp_only_section_selects_smtp_unchanged() {
        // A pre-issue-35 deployment: `[smtp]` and nothing else must
        // keep delivering through the relay exactly as before.
        let config = config_with_toml(
            r#"
            [smtp]
            host = "smtp.example.com"
            "#,
        );
        assert_eq!(
            config.mail_selection().unwrap(),
            MailSelection::Smtp(SmtpConfig {
                host: "smtp.example.com".into(),
                port: 587,
                username: None,
                password: None,
                from: None,
            })
        );
    }

    #[test]
    fn explicit_log_provider_wins_over_an_smtp_section() {
        let config = config_with_toml(
            r#"
            mail.provider = "log"

            [smtp]
            host = "smtp.example.com"
            "#,
        );
        assert_eq!(config.mail_selection().unwrap(), MailSelection::Log);
    }

    #[test]
    fn explicit_smtp_provider_uses_the_smtp_section() {
        let config = config_with_toml(
            r#"
            mail.provider = "smtp"

            [smtp]
            host = "smtp.example.com"
            "#,
        );
        assert!(matches!(
            config.mail_selection().unwrap(),
            MailSelection::Smtp(_)
        ));
    }

    #[test]
    fn smtp_provider_without_section_fails() {
        let config = config_with_toml(r#"mail.provider = "smtp""#);
        let err = config.mail_selection().unwrap_err().to_string();
        assert!(err.contains("requires an [smtp] section"), "got: {err}");
    }

    #[test]
    fn resend_section_detects_resend() {
        let config = config_with_toml(
            r#"
            [mail.resend]
            api_key = "re_test_key"
            from = "huddle@example.com"
            "#,
        );
        assert_eq!(
            config.mail_selection().unwrap(),
            MailSelection::Resend(resend_config())
        );
    }

    #[test]
    fn explicit_resend_provider_requires_the_section() {
        let config = config_with_toml(r#"mail.provider = "resend""#);
        let err = config.mail_selection().unwrap_err().to_string();
        assert!(
            err.contains("requires a [mail.resend] section"),
            "got: {err}"
        );
    }

    #[test]
    fn smtp_and_resend_without_provider_is_an_error() {
        // Both configured and no explicit pick must fail loudly, not
        // silently choose a transport.
        let config = config_with_toml(
            r#"
            [smtp]
            host = "smtp.example.com"

            [mail.resend]
            api_key = "re_test_key"
            from = "huddle@example.com"
            "#,
        );
        let err = config.mail_selection().unwrap_err().to_string();
        assert!(err.contains("set mail.provider"), "got: {err}");
    }

    #[test]
    fn empty_resend_fields_fail_validation() {
        let mut config = Config {
            mail: MailConfig {
                resend: Some(ResendConfig {
                    api_key: "  ".into(),
                    from: "huddle@example.com".into(),
                }),
                ..MailConfig::default()
            },
            ..Config::default()
        };
        assert!(config.mail_selection().is_err());
        config.mail.resend = Some(ResendConfig {
            api_key: "re_test_key".into(),
            from: String::new(),
        });
        assert!(config.mail_selection().is_err());
    }

    #[test]
    fn mail_provider_env_pins_the_transport() {
        let mut config = Config {
            smtp: Some(SmtpConfig {
                host: "smtp.example.com".into(),
                port: 587,
                username: None,
                password: None,
                from: None,
            }),
            ..Config::default()
        };
        Config::apply_mail_env_with(
            &mut config,
            MailEnv {
                provider: Some("log".into()),
                ..MailEnv::default()
            },
        )
        .unwrap();
        assert_eq!(config.mail_selection().unwrap(), MailSelection::Log);

        let mut config = Config::default();
        let err = Config::apply_mail_env_with(
            &mut config,
            MailEnv {
                provider: Some("carrier-pigeon".into()),
                ..MailEnv::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown mail provider"), "got: {err}");
    }

    #[test]
    fn resend_env_configures_and_merges() {
        let mut config = config_with_toml(
            r#"
            [mail.resend]
            api_key = "re_toml_key"
            from = "toml@example.com"
            "#,
        );
        Config::apply_mail_env_with(
            &mut config,
            MailEnv {
                resend_api_key: Some("re_env_key".into()),
                resend_from: None,
                ..MailEnv::default()
            },
        )
        .unwrap();
        // Env overrides only the fields it names; the TOML from stays.
        assert_eq!(
            config.mail_selection().unwrap(),
            MailSelection::Resend(ResendConfig {
                api_key: "re_env_key".into(),
                from: "toml@example.com".into(),
            })
        );

        // Half-configured via env alone must fail loudly.
        let mut config = Config::default();
        let err = Config::apply_mail_env_with(
            &mut config,
            MailEnv {
                resend_from: Some("env@example.com".into()),
                ..MailEnv::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("HUDDLE_MAIL_RESEND_API_KEY is required"),
            "got: {err}"
        );
    }

    #[test]
    fn smtp_env_configures_and_merges() {
        let mut config = Config::default();
        Config::apply_mail_env_with(
            &mut config,
            MailEnv {
                smtp_host: Some("smtp.example.com".into()),
                smtp_port: Some("2525".into()),
                smtp_from: Some("huddle@example.com".into()),
                ..MailEnv::default()
            },
        )
        .unwrap();
        assert_eq!(
            config.mail_selection().unwrap(),
            MailSelection::Smtp(SmtpConfig {
                host: "smtp.example.com".into(),
                port: 2525,
                username: None,
                password: None,
                from: Some("huddle@example.com".into()),
            })
        );

        // A garbage port fails loudly.
        let mut config = Config::default();
        let err = Config::apply_mail_env_with(
            &mut config,
            MailEnv {
                smtp_host: Some("smtp.example.com".into()),
                smtp_port: Some("not-a-port".into()),
                ..MailEnv::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("HUDDLE_SMTP_PORT"), "got: {err}");

        // Any SMTP env var without a host must not pass half-configured.
        let mut config = Config::default();
        let err = Config::apply_mail_env_with(
            &mut config,
            MailEnv {
                smtp_port: Some("2525".into()),
                ..MailEnv::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("HUDDLE_SMTP_HOST is required"), "got: {err}");
    }
}
