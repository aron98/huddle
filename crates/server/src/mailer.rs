//! Email delivery (issue #35, ADR 006): a pluggable transport behind
//! the [`MailTransport`] trait.
//!
//! [`MailTransport`] mirrors ADR 003's `IdentityProvider`: an
//! async-fn-in-trait, therefore not dyn-safe, so config selects a
//! concrete transport at startup via the [`Mailer`] enum — no trait
//! objects. Three transports exist: [`LogMailer`] (the default —
//! evaluating huddle must never require an email account, so the link
//! is logged instead of sent), [`SmtpMailer`] (the lettre STARTTLS
//! relay, `[smtp]`), and [`ResendMailer`] (the Resend HTTPS API,
//! `[mail.resend]`).
//!
//! Adding a provider means adding a type plus one arm in [`Mailer`];
//! callers — the magic-link flow today, meeting invites and reminders
//! tomorrow — only ever see [`Email`] and never change.

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::config::{Config, MailSelection, ResendConfig, SmtpConfig};

/// One email to deliver: everything a transport needs and nothing
/// provider-specific — the trait signature carries the message, not
/// per-provider knobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    /// Envelope recipient.
    pub to: String,
    pub subject: String,
    pub text: String,
}

impl Email {
    /// The magic sign-in link email of ADR 003's passwordless flow.
    pub fn login_link(to: &str, link: &str) -> Self {
        Self {
            to: to.to_owned(),
            subject: "Your huddle sign-in link".into(),
            text: format!(
                "Sign in to huddle:\n\n{link}\n\nThe link is single-use and expires soon.\n"
            ),
        }
    }
}

/// How the email was delivered — surfaced to tests and logs so they
/// can assert the path taken. Grows one variant per transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailResult {
    /// Written to the server log (no transport configured).
    Logged,
    /// Accepted by the configured SMTP relay.
    SmtpSent,
    /// Accepted by the Resend API.
    ResendSent,
}

/// The delivery boundary every caller codes against (ADR 006).
pub trait MailTransport {
    /// Deliver one email, reporting the path it took.
    async fn send(&self, email: Email) -> Result<MailResult, anyhow::Error>;
}

/// The dependency-free default: emails go to the log at `info` level.
/// The full text — magic link included — is printed on one line per
/// paragraph so e2e and local development can read the link straight
/// out of the server log.
pub struct LogMailer;

impl MailTransport for LogMailer {
    async fn send(&self, email: Email) -> Result<MailResult, anyhow::Error> {
        tracing::info!(
            "email to {} (no mail transport configured; logged, not emailed)\nsubject: {}\n{}",
            email.to,
            email.subject,
            email.text
        );
        Ok(MailResult::Logged)
    }
}

/// Delivers through a STARTTLS SMTP relay (lettre), configured by the
/// `[smtp]` section — the same path as before the abstraction.
pub struct SmtpMailer {
    config: SmtpConfig,
}

impl SmtpMailer {
    pub fn new(config: SmtpConfig) -> Self {
        Self { config }
    }
}

impl MailTransport for SmtpMailer {
    async fn send(&self, email: Email) -> Result<MailResult, anyhow::Error> {
        // Parse before touching the network so a bad address fails fast.
        let from: Mailbox = match &self.config.from {
            Some(address) => address
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid configured smtp from address: {e}"))?,
            None => "huddle@localhost"
                .parse()
                .map_err(|e| anyhow::anyhow!("fallback from address rejected: {e}"))?,
        };
        let to: Mailbox = email
            .to
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid recipient address: {e}"))?;

        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.config.host)?;
        builder = builder.port(self.config.port);
        if let (Some(username), Some(password)) = (&self.config.username, &self.config.password) {
            builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
        }
        let transport = builder.build();

        let message = lettre::Message::builder()
            .from(from)
            .to(to)
            .subject(email.subject)
            .body(email.text)?;

        transport.send(message).await?;
        Ok(MailResult::SmtpSent)
    }
}

/// The Resend send endpoint (docs.resend.com).
const RESEND_ENDPOINT: &str = "https://api.resend.com/emails";

/// Delivers through the Resend HTTPS API (issue #35): one
/// `POST /emails` call with a bearer key — no SDK, just a plain
/// `reqwest` client (already a dependency for OIDC), keeping the
/// binary small and the wire format auditable.
pub struct ResendMailer {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    from: String,
}

impl ResendMailer {
    pub fn new(config: &ResendConfig) -> anyhow::Result<Self> {
        Self::at(config, RESEND_ENDPOINT)
    }

    /// Build a client pointed at a specific endpoint — the seam the
    /// unit tests use to run against a local mock instead of the live
    /// API.
    fn at(config: &ResendConfig, endpoint: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            endpoint: endpoint.to_owned(),
            api_key: config.api_key.clone(),
            from: config.from.clone(),
        })
    }
}

impl MailTransport for ResendMailer {
    async fn send(&self, email: Email) -> Result<MailResult, anyhow::Error> {
        let payload = serde_json::json!({
            "from": self.from,
            "to": [email.to],
            "subject": email.subject,
            "text": email.text,
        });
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("reaching the Resend API: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Resend API rejected the email: {status} {body}");
        }
        Ok(MailResult::ResendSent)
    }
}

/// The configured transport, selected once at startup. The trait is
/// async-fn-in-trait and not dyn-safe, so — exactly like ADR 003's
/// `Identity` — this is a concrete enum, and each provider is one arm.
pub enum Mailer {
    Log(LogMailer),
    Smtp(SmtpMailer),
    Resend(ResendMailer),
}

impl Mailer {
    /// Build the transport the configuration selects. Selection and
    /// validation live in [`Config::mail_selection`] so config tests
    /// cover every combination; this only constructs the client.
    pub fn select(config: &Config) -> anyhow::Result<Self> {
        Ok(match config.mail_selection()? {
            MailSelection::Log => Self::Log(LogMailer),
            MailSelection::Smtp(smtp) => Self::Smtp(SmtpMailer::new(smtp)),
            MailSelection::Resend(resend) => Self::Resend(ResendMailer::new(&resend)?),
        })
    }
}

impl MailTransport for Mailer {
    async fn send(&self, email: Email) -> Result<MailResult, anyhow::Error> {
        match self {
            Self::Log(mailer) => mailer.send(email).await,
            Self::Smtp(mailer) => mailer.send(email).await,
            Self::Resend(mailer) => mailer.send(email).await,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use figment::Figment;
    use figment::providers::{Format, Serialized, Toml};
    use std::sync::{Arc, Mutex};

    fn config_with_toml(toml: &str) -> Config {
        Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(toml))
            .extract()
            .unwrap()
    }

    #[tokio::test]
    async fn unconfigured_mail_logs_the_link() {
        let mailer = Mailer::select(&Config::default()).unwrap();
        assert!(matches!(mailer, Mailer::Log(_)));
        let result = mailer
            .send(Email::login_link(
                "dev@example.com",
                "http://localhost:3000/api/auth/verify?t=1",
            ))
            .await
            .unwrap();
        assert_eq!(result, MailResult::Logged);
    }

    #[tokio::test]
    async fn invalid_recipient_is_rejected_without_network_io() {
        let mailer = SmtpMailer::new(SmtpConfig {
            host: "smtp.example.com".into(),
            port: 587,
            username: None,
            password: None,
            from: Some("huddle@example.com".into()),
        });
        let err = mailer
            .send(Email::login_link("not-an-email", "http://x/verify?t=1"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid recipient address"),
            "got: {err}"
        );
    }

    #[test]
    fn select_builds_the_transport_the_config_names() {
        let smtp_only = config_with_toml(
            r#"
            [smtp]
            host = "smtp.example.com"
            "#,
        );
        assert!(matches!(
            Mailer::select(&smtp_only).unwrap(),
            Mailer::Smtp(_)
        ));

        let resend = config_with_toml(
            r#"
            [mail]
            provider = "resend"

            [mail.resend]
            api_key = "re_test_key"
            from = "huddle@example.com"
            "#,
        );
        assert!(matches!(
            Mailer::select(&resend).unwrap(),
            Mailer::Resend(_)
        ));
    }

    // --- Resend, against an in-process mock of the API -------------------
    //
    // No live API is ever contacted from tests: the mock is an axum
    // router on an ephemeral loopback port that records each request
    // (authorization header + parsed JSON body) and answers with a
    // canned status/body.

    /// One request the mock received: its authorization header and
    /// parsed JSON body.
    #[derive(Clone)]
    struct RecordedRequest {
        authorization: Option<String>,
        body: serde_json::Value,
    }

    #[derive(Clone)]
    struct MockResend {
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        status: reqwest::StatusCode,
    }

    async fn start_mock_resend(status: u16, response: &'static str) -> (MockResend, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = MockResend {
            requests: Arc::new(Mutex::new(Vec::new())),
            status: reqwest::StatusCode::from_u16(status).unwrap(),
        };
        let handler = move |State(mock): State<MockResend>, headers: HeaderMap, body: String| async move {
            mock.requests.lock().unwrap().push(RecordedRequest {
                authorization: headers
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_owned()),
                body: serde_json::from_str(&body).unwrap_or_default(),
            });
            (mock.status, response)
        };
        let app = Router::new()
            .route("/emails", post(handler))
            .with_state(mock.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (mock, format!("http://{addr}/emails"))
    }

    #[tokio::test]
    async fn resend_delivers_via_the_api_and_reports_it() {
        let (mock, endpoint) = start_mock_resend(200, r#"{"id":"email-123"}"#).await;
        let mailer = ResendMailer::at(
            &ResendConfig {
                api_key: "re_test_key".into(),
                from: "huddle@example.com".into(),
            },
            &endpoint,
        )
        .unwrap();
        let email = Email::login_link(
            "dev@example.com",
            "http://localhost:3000/api/auth/verify?token=abc",
        );
        assert_eq!(mailer.send(email).await.unwrap(), MailResult::ResendSent);

        let (auth, body) = {
            let req = mock.requests.lock().unwrap().first().unwrap().clone();
            (req.authorization, req.body)
        };
        assert_eq!(auth.as_deref(), Some("Bearer re_test_key"));
        assert_eq!(body["from"], "huddle@example.com");
        assert_eq!(body["to"][0], "dev@example.com");
        assert_eq!(body["subject"], "Your huddle sign-in link");
        assert!(
            body["text"]
                .as_str()
                .unwrap()
                .contains("http://localhost:3000/api/auth/verify?token=abc")
        );
    }

    #[tokio::test]
    async fn resend_api_errors_are_surfaced() {
        let (mock, endpoint) = start_mock_resend(422, r#"{"message":"Validation error"}"#).await;
        let mailer = ResendMailer::at(
            &ResendConfig {
                api_key: "re_test_key".into(),
                from: "huddle@example.com".into(),
            },
            &endpoint,
        )
        .unwrap();
        let err = mailer
            .send(Email::login_link("dev@example.com", "http://x/verify?t=1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("422"), "got: {err}");
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
    }
}
