//! Magic-link delivery: SMTP via lettre when configured, log otherwise.//!
//! Self-hosting should not require an SMTP relay to evaluate huddle, so
//! the default mode logs the sign-in link at `info` level instead of
//! sending mail. Setting `[smtp]` in the config switches to real email.

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::SmtpConfig;

/// How the link reached the user — surfaced to tests and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailResult {
    /// Sent through the configured SMTP relay.
    Sent,
    /// Written to the server log (no SMTP configured).
    Logged,
}

/// Deliver one magic sign-in link.
///
/// `None` SMTP logs the link — the development and dependency-free mode.
pub async fn send_login_link(
    smtp: Option<&SmtpConfig>,
    to: &str,
    link: &str,
) -> Result<MailResult, anyhow::Error> {
    let Some(smtp) = smtp else {
        tracing::info!(
            "magic link for {to}: {link} (SMTP not configured; link logged, not emailed)"
        );
        return Ok(MailResult::Logged);
    };

    // Parse before touching the network so a bad address fails fast.
    let from: Mailbox = match &smtp.from {
        Some(address) => address
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid configured smtp from address: {e}"))?,
        None => "huddle@localhost"
            .parse()
            .map_err(|e| anyhow::anyhow!("fallback from address rejected: {e}"))?,
    };
    let to: Mailbox = to
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid recipient address: {e}"))?;

    let mut builder = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host)?;
    builder = builder.port(smtp.port);
    if let (Some(username), Some(password)) = (&smtp.username, &smtp.password) {
        builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
    }
    let transport = builder.build();

    let message = Message::builder()
        .from(from)
        .to(to)
        .subject("Your huddle sign-in link")
        .body(format!(
            "Sign in to huddle:\n\n{link}\n\nThe link is single-use and expires soon.\n"
        ))?;

    transport.send(message).await?;
    Ok(MailResult::Sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unconfigured_smtp_logs_the_link() {
        let result = send_login_link(None, "dev@example.com", "http://localhost:3000/verify?t=1")
            .await
            .unwrap();
        assert_eq!(result, MailResult::Logged);
    }

    #[tokio::test]
    async fn invalid_recipient_is_rejected_without_network_io() {
        let smtp = SmtpConfig {
            host: "smtp.example.com".into(),
            port: 587,
            username: None,
            password: None,
            from: Some("huddle@example.com".into()),
        };
        let err = send_login_link(Some(&smtp), "not-an-email", "http://x/verify?t=1")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid recipient address"),
            "got: {err}"
        );
    }
}
