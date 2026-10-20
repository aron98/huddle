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
    /// TLS termination (issue #30, ADR 008). `None` (the default)
    /// serves plain HTTP — development and tests only. See
    /// [`TlsConfig`].
    pub tls: Option<TlsConfig>,
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

/// TLS termination (issue #30, ADR 008), as configured in `huddle.toml`:
///
/// ```toml
/// # Automatic certificates via Let's Encrypt (TLS-ALPN-01):
/// [tls]
/// mode = "acme"
///
/// [tls.acme]
/// domains = ["meet.example.com"]
/// contact = "admin@example.com" # optional, expiration mail from LE
/// staging = false               # rehearse against the LE staging CA
///
/// # Or bring your own certificate chain:
/// [tls]
/// mode = "static"
///
/// [tls.certs]
/// cert = "/etc/huddle/fullchain.pem"
/// key = "/etc/huddle/privkey.pem"
/// ```
///
/// No `[tls]` section serves plain HTTP — development and tests only.
/// `mode` may be omitted when exactly one of `[tls.acme]` / `[tls.certs]`
/// is present; an explicit `mode` wins over detection, and both sections
/// without one is an error rather than a silent pick.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TlsConfig {
    /// Pins the mode when set; `None` auto-detects from the sections.
    pub mode: Option<TlsMode>,
    /// Automatic certificates via Let's Encrypt; required when the mode
    /// is (or detects to) `acme`.
    pub acme: Option<AcmeTlsConfig>,
    /// PEM files on disk; required when the mode is (or detects to)
    /// `static`.
    pub certs: Option<StaticTlsConfig>,
}

/// Which kind of TLS termination to run, e.g. from `tls.mode` or
/// `HUDDLE_TLS_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Automatic certificates via Let's Encrypt (TLS-ALPN-01).
    Acme,
    /// A certificate chain and key from PEM files on disk.
    Static,
}

impl TlsMode {
    /// Parse a mode name, e.g. from `HUDDLE_TLS_MODE`. A typo fails
    /// loudly, naming the valid values.
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim() {
            "acme" => Ok(Self::Acme),
            "static" => Ok(Self::Static),
            other => {
                anyhow::bail!("unknown TLS mode {other:?}; valid values are \"acme\", \"static\"")
            }
        }
    }
}

/// Let's Encrypt automation, as configured in `huddle.toml`:
///
/// ```toml
/// [tls.acme]
/// domains = ["meet.example.com"]
/// contact = "admin@example.com"
/// staging = false
/// ```
///
/// The certificate cache lives under `data_dir`; the domain must point
/// at this server and port 443 (or whichever port `bind` uses) must be
/// reachable for the TLS-ALPN-01 challenge.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AcmeTlsConfig {
    /// Domains to request certificates for; at least one is required.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Contact email forwarded to Let's Encrypt (expiry notices).
    #[serde(default)]
    pub contact: Option<String>,
    /// Use the Let's Encrypt *staging* directory while rehearsing;
    /// production rate limits are unforgiving of experiments.
    #[serde(default)]
    pub staging: bool,
}

/// A certificate chain and key from PEM files on disk, as configured in
/// `huddle.toml`:
///
/// ```toml
/// [tls.certs]
/// cert = "/etc/huddle/fullchain.pem"
/// key = "/etc/huddle/privkey.pem"
/// ```
///
/// Or per-field via `HUDDLE_TLS_CERT` / `HUDDLE_TLS_KEY`. Both files
/// must exist at startup — a missing file fails fast instead of at the
/// first handshake.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StaticTlsConfig {
    /// PEM certificate chain (leaf first, then intermediates).
    pub cert: PathBuf,
    /// PEM private key matching the leaf certificate.
    pub key: PathBuf,
}

/// The TLS setup a configuration resolves to (issue #30, ADR 008):
/// decided and validated here — pure data, no keys loaded — so config
/// tests can cover every combination without touching the network or
/// the filesystem beyond an existence check. Startup turns it into a
/// listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsSelection {
    /// Automatic certificates via Let's Encrypt.
    Acme {
        domains: Vec<String>,
        contact: Option<String>,
        staging: bool,
    },
    /// PEM files on disk.
    Static { cert: PathBuf, key: PathBuf },
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
            tls: None,
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
        Self::apply_tls_env_with(&mut config, TlsEnv::from_env())?;
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

    /// Resolve and validate the TLS setup (issue #30, ADR 008),
    /// mirroring [`Config::mail_selection`].
    ///
    /// `None` (no `[tls]` section) means plain HTTP — development and
    /// tests only; the server logs a warning. An explicit `mode` wins
    /// and requires its section, without one the mode is detected from
    /// whichever section is present, and both configured at once is an
    /// error rather than a silent pick.
    pub fn tls_selection(&self) -> anyhow::Result<Option<TlsSelection>> {
        let Some(tls) = &self.tls else {
            return Ok(None);
        };
        if let Some(mode) = tls.mode {
            return match mode {
                TlsMode::Acme => {
                    if tls.certs.is_some() {
                        anyhow::bail!(
                            "tls.mode = \"acme\" conflicts with a [tls.certs] \
                             section; remove one of them"
                        );
                    }
                    Ok(Some(self.selected_acme()?))
                }
                TlsMode::Static => {
                    if tls.acme.is_some() {
                        anyhow::bail!(
                            "tls.mode = \"static\" conflicts with a [tls.acme] \
                             section; remove one of them"
                        );
                    }
                    Ok(Some(self.selected_static()?))
                }
            };
        }
        match (&tls.acme, &tls.certs) {
            (Some(_), Some(_)) => anyhow::bail!(
                "[tls.acme] and [tls.certs] are both configured; \
                 set tls.mode to pick one"
            ),
            (Some(_), None) => Ok(Some(self.selected_acme()?)),
            (None, Some(_)) => Ok(Some(self.selected_static()?)),
            (None, None) => anyhow::bail!(
                "[tls] is configured but empty; add [tls.acme] or \
                 [tls.certs], or set tls.mode"
            ),
        }
    }

    /// Validate `[tls.acme]` and select it — shared by the explicit
    /// `mode = "acme"` arm and acme detection.
    fn selected_acme(&self) -> anyhow::Result<TlsSelection> {
        let Some(acme) = self.tls.as_ref().and_then(|t| t.acme.as_ref()) else {
            anyhow::bail!("TLS mode \"acme\" requires a [tls.acme] section");
        };
        if acme.domains.iter().all(|d| d.trim().is_empty()) {
            anyhow::bail!("[tls.acme] domains must list at least one domain");
        }
        Ok(TlsSelection::Acme {
            domains: acme.domains.clone(),
            contact: acme.contact.clone(),
            staging: acme.staging,
        })
    }

    /// Validate `[tls.certs]` and select it — shared by the explicit
    /// `mode = "static"` arm and static detection. The files must
    /// already exist: a missing certificate is a deployment mistake
    /// worth failing fast on, not a first-handshake problem.
    fn selected_static(&self) -> anyhow::Result<TlsSelection> {
        let Some(certs) = self.tls.as_ref().and_then(|t| t.certs.as_ref()) else {
            anyhow::bail!("TLS mode \"static\" requires a [tls.certs] section");
        };
        for (kind, path) in [("cert", &certs.cert), ("key", &certs.key)] {
            if !path.is_file() {
                anyhow::bail!("TLS {kind} file {} does not exist", path.display());
            }
        }
        Ok(TlsSelection::Static {
            cert: certs.cert.clone(),
            key: certs.key.clone(),
        })
    }

    /// Whether session cookies should carry the `Secure` attribute.
    ///
    /// Any TLS termination in play — built-in (`[tls]`) or a proxy in
    /// front (an `https://` `base_url`) — means browsers must refuse to
    /// send the cookie over plain HTTP. Plain-HTTP dev setups (and the
    /// e2e suite, which talks to 127.0.0.1 over HTTP) keep working.
    pub fn secure_cookies(&self) -> bool {
        self.tls.is_some() || self.base_url.starts_with("https://")
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

    /// Testable core of the `HUDDLE_TLS_*` env handling, mirroring
    /// [`Config::apply_mail_env_with`]: each set, non-empty value
    /// overrides the matching `[tls]` TOML field, merging into an
    /// existing section when there is one. A half-configured mode must
    /// fail loudly rather than silently break TLS; cross-field checks
    /// (mode conflicts, file existence) live in
    /// [`Config::tls_selection`], which runs on every startup anyway.
    fn apply_tls_env_with(&mut self, env: TlsEnv) -> anyhow::Result<()> {
        let any_set = env.mode.is_some()
            || env.domains.is_some()
            || env.contact.is_some()
            || env.staging.is_some()
            || env.cert.is_some()
            || env.key.is_some();
        if !any_set {
            return Ok(());
        }
        let mut tls = self.tls.clone().unwrap_or_default();

        if let Some(mode) = env.mode.filter(|v| !v.trim().is_empty()) {
            tls.mode = Some(TlsMode::parse(&mode)?);
        }

        if env.domains.is_some() || env.contact.is_some() || env.staging.is_some() {
            let mut acme = tls.acme.clone().unwrap_or(AcmeTlsConfig {
                domains: Vec::new(),
                contact: None,
                staging: false,
            });
            if let Some(v) = env.domains {
                acme.domains = parse_tls_domains(&v)?;
            }
            if let Some(v) = env.contact.filter(|v| !v.trim().is_empty()) {
                acme.contact = Some(v);
            }
            if let Some(v) = env.staging {
                acme.staging = v.trim().parse().map_err(|_| {
                    anyhow::anyhow!("HUDDLE_TLS_STAGING must be \"true\" or \"false\", got {v:?}")
                })?;
            }
            if acme.domains.is_empty() {
                anyhow::bail!(
                    "HUDDLE_TLS_DOMAINS is required when any \
                     HUDDLE_TLS_DOMAINS/CONTACT/STAGING variable is set"
                );
            }
            tls.acme = Some(acme);
        }

        if env.cert.is_some() || env.key.is_some() {
            let mut certs = tls.certs.clone().unwrap_or(StaticTlsConfig {
                cert: PathBuf::new(),
                key: PathBuf::new(),
            });
            if let Some(v) = env.cert.filter(|v| !v.trim().is_empty()) {
                certs.cert = PathBuf::from(v);
            }
            if let Some(v) = env.key.filter(|v| !v.trim().is_empty()) {
                certs.key = PathBuf::from(v);
            }
            if certs.cert.as_os_str().is_empty() || certs.key.as_os_str().is_empty() {
                anyhow::bail!(
                    "HUDDLE_TLS_CERT and HUDDLE_TLS_KEY are both required \
                     when either is set"
                );
            }
            tls.certs = Some(certs);
        }

        self.tls = Some(tls);
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

/// The `HUDDLE_TLS_*` variables, collected for
/// [`Config::apply_tls_env_with`]. Figment's flat env keys cannot reach
/// nested sections (see the OIDC note on `Config::load`), so — mirroring
/// `HUDDLE_OIDC_*` and `HUDDLE_MAIL_*` — they are applied explicitly
/// after extraction.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TlsEnv {
    mode: Option<String>,
    domains: Option<String>,
    contact: Option<String>,
    staging: Option<String>,
    cert: Option<String>,
    key: Option<String>,
}

impl TlsEnv {
    /// Collect the variables from the process environment.
    fn from_env() -> Self {
        let var = |key: &str| std::env::var_os(key).map(|v| v.to_string_lossy().into_owned());
        Self {
            mode: var("HUDDLE_TLS_MODE"),
            domains: var("HUDDLE_TLS_DOMAINS"),
            contact: var("HUDDLE_TLS_CONTACT"),
            staging: var("HUDDLE_TLS_STAGING"),
            cert: var("HUDDLE_TLS_CERT"),
            key: var("HUDDLE_TLS_KEY"),
        }
    }
}

/// Parse `HUDDLE_TLS_DOMAINS`: a JSON-style array — the same value
/// syntax figment itself uses for `HUDDLE_ICE_SERVERS` — or a bare
/// single domain as a convenience.
fn parse_tls_domains(value: &str) -> anyhow::Result<Vec<String>> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).map_err(|_| {
            anyhow::anyhow!(
                "HUDDLE_TLS_DOMAINS must be an array like '[\"meet.example.com\"]', \
                 got {value:?}"
            )
        })
    } else {
        Ok(vec![trimmed.to_owned()])
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
        assert_eq!(config.tls, None);
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

    // --- TLS termination (issue #30) -----------------------------------

    #[test]
    fn no_tls_section_selects_plain_http() {
        let config = config_with_toml("");
        assert_eq!(config.tls, None);
        assert!(config.tls_selection().unwrap().is_none());
        // Dev default: plain HTTP, no Secure cookie attribute.
        assert!(!config.secure_cookies());
    }

    #[test]
    fn acme_section_parses_and_selects() {
        let config = config_with_toml(
            r#"
            [tls.acme]
            domains = ["meet.example.com", "alt.example.com"]
            contact = "admin@example.com"
            staging = true
            "#,
        );
        assert_eq!(
            config.tls_selection().unwrap(),
            Some(TlsSelection::Acme {
                domains: vec!["meet.example.com".into(), "alt.example.com".into()],
                contact: Some("admin@example.com".into()),
                staging: true,
            })
        );
        assert!(config.secure_cookies());
    }

    #[test]
    fn acme_mode_requires_its_section_and_domains() {
        let err = config_with_toml(
            r#"
            [tls]
            mode = "acme"
            "#,
        )
        .tls_selection()
        .unwrap_err()
        .to_string();
        assert!(err.contains("requires a [tls.acme] section"), "got: {err}");

        let err = config_with_toml(
            r#"
            [tls]
            mode = "acme"

            [tls.acme]
            domains = []
            "#,
        )
        .tls_selection()
        .unwrap_err()
        .to_string();
        assert!(err.contains("at least one domain"), "got: {err}");
    }

    #[test]
    fn static_section_selects_when_files_exist() {
        let dir =
            std::env::temp_dir().join(format!("huddle-tls-config-{}-static", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, b"not really a cert").unwrap();
        std::fs::write(&key, b"not really a key").unwrap();

        let config = config_with_toml(&format!(
            r#"
            [tls.certs]
            cert = {:?}
            key = {:?}
            "#,
            cert.display(),
            key.display()
        ));
        assert_eq!(
            config.tls_selection().unwrap(),
            Some(TlsSelection::Static { cert, key })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn static_selection_fails_fast_on_missing_files() {
        let config = config_with_toml(
            r#"
            [tls.certs]
            cert = "/nonexistent/cert.pem"
            key = "/nonexistent/key.pem"
            "#,
        );
        let err = config.tls_selection().unwrap_err().to_string();
        assert!(err.contains("does not exist"), "got: {err}");
        assert!(err.contains("cert"), "got: {err}");
    }

    #[test]
    fn both_tls_sections_conflict_without_mode() {
        let err = config_with_toml(
            r#"
            [tls.acme]
            domains = ["meet.example.com"]

            [tls.certs]
            cert = "cert.pem"
            key = "key.pem"
            "#,
        )
        .tls_selection()
        .unwrap_err()
        .to_string();
        assert!(err.contains("set tls.mode to pick one"), "got: {err}");
    }

    #[test]
    fn explicit_tls_mode_wins_and_conflicting_section_fails() {
        // A cert file must exist for the winning side to validate; the
        // conflicting section is rejected before that even matters.
        let err = config_with_toml(
            r#"
            [tls]
            mode = "acme"

            [tls.acme]
            domains = ["meet.example.com"]

            [tls.certs]
            cert = "cert.pem"
            key = "key.pem"
            "#,
        )
        .tls_selection()
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("conflicts with a [tls.certs] section"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_tls_section_fails() {
        let err = config_with_toml("[tls]")
            .tls_selection()
            .unwrap_err()
            .to_string();
        assert!(err.contains("[tls] is configured but empty"), "got: {err}");
    }

    #[test]
    fn tls_env_configures_acme() {
        let mut config = Config::default();
        Config::apply_tls_env_with(
            &mut config,
            TlsEnv {
                mode: Some("acme".into()),
                domains: Some(r#"["meet.example.com"]"#.into()),
                contact: Some("admin@example.com".into()),
                staging: Some("true".into()),
                ..TlsEnv::default()
            },
        )
        .unwrap();
        assert_eq!(
            config.tls_selection().unwrap(),
            Some(TlsSelection::Acme {
                domains: vec!["meet.example.com".into()],
                contact: Some("admin@example.com".into()),
                staging: true,
            })
        );
    }

    #[test]
    fn tls_env_accepts_bare_single_domain() {
        let mut config = Config::default();
        Config::apply_tls_env_with(
            &mut config,
            TlsEnv {
                domains: Some("meet.example.com".into()),
                ..TlsEnv::default()
            },
        )
        .unwrap();
        assert_eq!(
            config.tls_selection().unwrap(),
            Some(TlsSelection::Acme {
                domains: vec!["meet.example.com".into()],
                contact: None,
                staging: false,
            })
        );
    }

    #[test]
    fn tls_env_merges_into_toml_per_field() {
        let config = config_with_toml(
            r#"
            [tls.acme]
            domains = ["toml.example.com"]
            contact = "toml@example.com"
            "#,
        );
        let mut config = config;
        Config::apply_tls_env_with(
            &mut config,
            TlsEnv {
                domains: Some(r#"["env.example.com"]"#.into()),
                staging: Some("true".into()),
                ..TlsEnv::default()
            },
        )
        .unwrap();
        assert_eq!(
            config.tls_selection().unwrap(),
            Some(TlsSelection::Acme {
                domains: vec!["env.example.com".into()],
                contact: Some("toml@example.com".into()),
                staging: true,
            })
        );
    }

    #[test]
    fn half_configured_tls_env_fails() {
        // Domains are the one required acme field.
        let mut config = Config::default();
        let err = Config::apply_tls_env_with(
            &mut config,
            TlsEnv {
                contact: Some("admin@example.com".into()),
                ..TlsEnv::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("HUDDLE_TLS_DOMAINS is required"), "got: {err}");

        // Cert and key are a pair.
        let mut config = Config::default();
        let err = Config::apply_tls_env_with(
            &mut config,
            TlsEnv {
                cert: Some("/cert.pem".into()),
                ..TlsEnv::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("both required"), "got: {err}");

        // A typo'd mode names the valid values.
        let mut config = Config::default();
        let err = Config::apply_tls_env_with(
            &mut config,
            TlsEnv {
                mode: Some("acme-cert".into()),
                ..TlsEnv::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown TLS mode"), "got: {err}");

        // A non-boolean staging flag fails loudly, named.
        let mut config = Config::default();
        let err = Config::apply_tls_env_with(
            &mut config,
            TlsEnv {
                domains: Some("meet.example.com".into()),
                staging: Some("yes".into()),
                ..TlsEnv::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("HUDDLE_TLS_STAGING"), "got: {err}");

        // A malformed domains array fails loudly, named.
        let mut config = Config::default();
        let err = Config::apply_tls_env_with(
            &mut config,
            TlsEnv {
                domains: Some(r#"["unterminated"#.into()),
                ..TlsEnv::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("HUDDLE_TLS_DOMAINS"), "got: {err}");
    }

    #[test]
    fn secure_cookies_follows_tls_and_base_url() {
        // Dev default over plain HTTP: no Secure attribute (e2e runs
        // against 127.0.0.1 over HTTP and must keep working).
        assert!(!Config::default().secure_cookies());

        // Behind an HTTPS reverse proxy without built-in TLS.
        let config = config_with_toml(r#"base_url = "https://meet.example.com""#);
        assert!(config.secure_cookies());

        // Built-in TLS, even with a plain-HTTP base_url left behind.
        let config = config_with_toml(
            r#"
            [tls.acme]
            domains = ["meet.example.com"]
            "#,
        );
        assert!(config.secure_cookies());

        // No TLS and a plain-HTTP base_url stays non-secure.
        let config = config_with_toml(r#"base_url = "http://localhost:3000""#);
        assert!(!config.secure_cookies());
    }
}
