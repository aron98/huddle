use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::engine::EngineHandle;
use crate::identity::{Identity, MagicLinkIdentityProvider, OidcIdentityProvider};
use crate::mailer::Mailer;
use crate::ratelimit::RateLimiter;

/// Shared application state, cheap to clone into handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

pub struct Inner {
    pub config: Arc<Config>,
    pub pool: SqlitePool,
    pub engine: EngineHandle,
    /// The configured identity flow: magic links by default, OIDC SSO
    /// when `[oidc]` is set. Concrete enum, not a trait object — the
    /// `IdentityProvider` trait is async-fn-in-trait and not dyn-safe.
    pub identity: Identity,
    /// The configured mail transport: log by default, SMTP or Resend
    /// when configured (issue #35). Concrete enum for the same reason
    /// as `identity` (ADR 006).
    pub mailer: Mailer,
    /// HMAC key for session cookies. Random per boot unless
    /// `session_secret` is configured.
    pub session_key: Vec<u8>,
    /// Per-IP sliding window limiter for auth requests and new
    /// websocket connections (rate_limit_per_min, 0 disables).
    pub rate_limiter: RateLimiter,
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        pool: SqlitePool,
        engine: EngineHandle,
    ) -> anyhow::Result<Self> {
        // Magic-link TTL doubles as the OIDC login-state TTL: both are
        // the "how long may this sign-in attempt live" knob.
        let identity = match &config.oidc {
            Some(oidc) => Identity::Oidc(Box::new(OidcIdentityProvider::new(
                pool.clone(),
                oidc,
                &config.base_url,
                time::Duration::minutes(i64::from(config.login_token_ttl_min)),
            )?)),
            None => Identity::MagicLinks(MagicLinkIdentityProvider::new(
                pool.clone(),
                time::Duration::minutes(i64::from(config.login_token_ttl_min)),
            )),
        };
        let mailer = Mailer::select(&config)?;
        let session_key = match &config.session_secret {
            Some(secret) => secret.as_bytes().to_vec(),
            None => {
                tracing::warn!(
                    "session secret not configured; using a random key — \
                     sessions will not survive restarts (set HUDDLE_SESSION_SECRET)"
                );
                rand::random::<[u8; 32]>().to_vec()
            }
        };
        let rate_limiter = RateLimiter::new(config.rate_limit_per_min, Duration::from_secs(60));
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                pool,
                engine,
                identity,
                mailer,
                session_key,
                rate_limiter,
            }),
        })
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.inner.pool
    }

    pub fn engine(&self) -> &EngineHandle {
        &self.inner.engine
    }

    pub fn identity(&self) -> &Identity {
        &self.inner.identity
    }

    pub fn mailer(&self) -> &Mailer {
        &self.inner.mailer
    }

    pub fn session_key(&self) -> &[u8] {
        &self.inner.session_key
    }

    pub fn rate_limiter(&self) -> &RateLimiter {
        &self.inner.rate_limiter
    }
}
