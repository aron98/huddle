//! Organizer identity (ADR 003): passwordless magic links.
//!
//! [`IdentityProvider`] is the boundary the rest of the server codes
//! against; SSO (OIDC/SAML) will arrive as a second implementation of the
//! trait, not a rewrite. The MVP implementation,
//! [`MagicLinkIdentityProvider`], mints single-use tokens whose SHA-256
//! hashes live in `login_tokens`.
//!
//! The auth routes that call into this module land in a follow-up commit;
//! the allowance drops when they do.
#![allow(dead_code)]

use rand::RngExt;
use sha2::{Digest, Sha256};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::meetings::{from_text, to_text};
/// A verified organizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organizer {
    pub id: Uuid,
    pub email: String,
}

/// Identity flow failures.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("invalid or expired login token")]
    InvalidToken,
    #[error("invalid email address")]
    InvalidEmail,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
    #[error("corrupt identity data: {0}")]
    Corrupt(String),
}

/// How organizers prove who they are (ADR 003). Guests never touch this.
pub trait IdentityProvider {
    /// Mint a one-time login token for `email`, creating the organizer on
    /// first login. Returns the raw token; building the verify URL and
    /// delivering it is the caller's business (see `mailer`).
    async fn start_login(&self, email: &str, now: OffsetDateTime) -> Result<String, IdentityError>;

    /// Exchange a one-time token for its organizer. Tokens are
    /// single-use: a second call with the same token must fail.
    async fn verify(&self, token: &str, now: OffsetDateTime) -> Result<Organizer, IdentityError>;
}

/// The MVP [`IdentityProvider`]: one-time tokens backed by SQLite.
pub struct MagicLinkIdentityProvider {
    pool: sqlx::SqlitePool,
    /// How long a fresh login token stays valid (ADR 003: short).
    ttl: time::Duration,
}

impl MagicLinkIdentityProvider {
    pub fn new(pool: sqlx::SqlitePool, ttl: time::Duration) -> Self {
        Self { pool, ttl }
    }
}

impl IdentityProvider for MagicLinkIdentityProvider {
    async fn start_login(&self, email: &str, now: OffsetDateTime) -> Result<String, IdentityError> {
        let email = email.trim();
        if !valid_email(email) {
            return Err(IdentityError::InvalidEmail);
        }

        let organizer_id = ensure_organizer(&self.pool, email, now).await?;

        // 32 random bytes, hex-encoded: 64 URL-safe characters. Only the
        // SHA-256 hash is stored, so a leaked database cannot log anyone
        // in.
        let token: [u8; 32] = rand::rng().random();
        let token = hex(&token);
        sqlx::query("INSERT INTO login_tokens (hash, organizer_id, expires_at, used) VALUES (?1, ?2, ?3, 0)")
            .bind(hash_token(&token))
            .bind(organizer_id.to_string())
            .bind(to_text(now + self.ttl).map_err(|e| IdentityError::Corrupt(e.to_string()))?)
            .execute(&self.pool)
            .await?;
        Ok(token)
    }

    async fn verify(&self, token: &str, now: OffsetDateTime) -> Result<Organizer, IdentityError> {
        let token = token.trim();
        let row = sqlx::query(
            "SELECT t.used, t.expires_at, o.id, o.email \
             FROM login_tokens t JOIN organizers o ON o.id = t.organizer_id \
             WHERE t.hash = ?1",
        )
        .bind(hash_token(token))
        .fetch_optional(&self.pool)
        .await?
        .ok_or(IdentityError::InvalidToken)?;

        let used: i64 = row
            .try_get("used")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        let expires_at: String = row
            .try_get("expires_at")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        if used != 0 {
            return Err(IdentityError::InvalidToken);
        }
        let expires_at = from_text(&expires_at, "expires_at")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        if expires_at <= now {
            return Err(IdentityError::InvalidToken);
        }

        // Mark single-use first, atomically: a concurrent verify racing us
        // loses on the `used = 0` guard.
        let claimed = sqlx::query("UPDATE login_tokens SET used = 1 WHERE hash = ?1 AND used = 0")
            .bind(hash_token(token))
            .execute(&self.pool)
            .await?;
        if claimed.rows_affected() != 1 {
            return Err(IdentityError::InvalidToken);
        }

        let id: String = row
            .try_get("id")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        let email: String = row
            .try_get("email")
            .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
        Ok(Organizer {
            id: Uuid::parse_str(&id).map_err(|e| IdentityError::Corrupt(e.to_string()))?,
            email,
        })
    }
}

/// Create the organizer on first login, return its id either way.
async fn ensure_organizer(
    pool: &sqlx::SqlitePool,
    email: &str,
    now: OffsetDateTime,
) -> Result<Uuid, IdentityError> {
    sqlx::query("INSERT INTO organizers (id, email, created_at) VALUES (?1, ?2, ?3) ON CONFLICT (email) DO NOTHING")
        .bind(Uuid::new_v4().to_string())
        .bind(email)
        .bind(to_text(now).map_err(|e| IdentityError::Corrupt(e.to_string()))?)
        .execute(pool)
        .await?;
    let id: String = sqlx::query("SELECT id FROM organizers WHERE email = ?1")
        .bind(email)
        .fetch_one(pool)
        .await?
        .try_get("id")
        .map_err(|e| IdentityError::Corrupt(e.to_string()))?;
    Uuid::parse_str(&id).map_err(|e| IdentityError::Corrupt(e.to_string()))
}

/// Deliberately light: an RFC 5322 parser is not worth the dependency.
/// Must be non-empty, contain `@` with non-empty local and domain parts,
/// and stay within the 254-character limit.
fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && email.len() <= 254
        && !email.chars().any(char::is_whitespace)
}

fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    async fn provider() -> (MagicLinkIdentityProvider, sqlx::SqlitePool) {
        let pool = crate::db::test_pool().await;
        (
            MagicLinkIdentityProvider::new(pool.clone(), time::Duration::minutes(15)),
            pool,
        )
    }

    #[tokio::test]
    async fn magic_link_logins_create_the_organizer_on_first_use() {
        let (provider, _pool) = provider().await;
        let now = OffsetDateTime::now_utc();

        let token = provider
            .start_login("alice@example.com", now)
            .await
            .unwrap();
        let organizer = provider.verify(&token, now).await.unwrap();
        assert_eq!(organizer.email, "alice@example.com");

        // A second login mints a fresh token for the same organizer.
        let token = provider
            .start_login("alice@example.com", now)
            .await
            .unwrap();
        let again = provider.verify(&token, now).await.unwrap();
        assert_eq!(again.id, organizer.id);
    }

    #[tokio::test]
    async fn magic_links_are_single_use() {
        let (provider, _pool) = provider().await;
        let now = OffsetDateTime::now_utc();
        let token = provider.start_login("bob@example.com", now).await.unwrap();

        provider.verify(&token, now).await.unwrap();
        assert!(matches!(
            provider.verify(&token, now).await,
            Err(IdentityError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn magic_links_expire() {
        let (provider, _pool) = provider().await;
        let now = OffsetDateTime::now_utc();
        let token = provider
            .start_login("carol@example.com", now)
            .await
            .unwrap();

        let later = now + time::Duration::minutes(16);
        assert!(matches!(
            provider.verify(&token, later).await,
            Err(IdentityError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn unknown_and_malformed_tokens_are_rejected() {
        let (provider, _pool) = provider().await;
        let now = OffsetDateTime::now_utc();
        assert!(matches!(
            provider.verify("not-a-token", now).await,
            Err(IdentityError::InvalidToken)
        ));
        assert!(matches!(
            provider.start_login("no-at-sign", now).await,
            Err(IdentityError::InvalidEmail)
        ));
        assert!(matches!(
            provider.start_login("double@@at", now).await,
            Err(IdentityError::InvalidEmail)
        ));
    }

    #[tokio::test]
    async fn only_the_hash_is_stored() {
        let (provider, pool) = provider().await;
        let now = OffsetDateTime::now_utc();
        let token = provider.start_login("dave@example.com", now).await.unwrap();

        // The token itself must never appear in the database.
        let row: String = sqlx::query("SELECT hash FROM login_tokens")
            .fetch_one(&pool)
            .await
            .unwrap()
            .try_get("hash")
            .unwrap();
        assert_eq!(row, hash_token(&token));
        assert_ne!(row, token);
    }
}
