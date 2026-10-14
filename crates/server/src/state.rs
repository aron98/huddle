use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::engine::EngineHandle;
use crate::identity::MagicLinkIdentityProvider;
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
    pub identity: MagicLinkIdentityProvider,
    /// HMAC key for session cookies. Random per boot unless
    /// `session_secret` is configured.
    pub session_key: Vec<u8>,
    /// Per-IP sliding window limiter for auth requests and new
    /// websocket connections (rate_limit_per_min, 0 disables).
    pub rate_limiter: RateLimiter,
}

impl AppState {
    pub fn new(config: Arc<Config>, pool: SqlitePool, engine: EngineHandle) -> Self {
        let identity = MagicLinkIdentityProvider::new(
            pool.clone(),
            time::Duration::minutes(i64::from(config.login_token_ttl_min)),
        );
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
        Self {
            inner: Arc::new(Inner {
                config,
                pool,
                engine,
                identity,
                session_key,
                rate_limiter,
            }),
        }
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

    pub fn identity(&self) -> &MagicLinkIdentityProvider {
        &self.inner.identity
    }

    pub fn session_key(&self) -> &[u8] {
        &self.inner.session_key
    }

    pub fn rate_limiter(&self) -> &RateLimiter {
        &self.inner.rate_limiter
    }
}
