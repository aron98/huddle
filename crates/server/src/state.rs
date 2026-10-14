use std::sync::Arc;

use sqlx::SqlitePool;

use crate::config::Config;
use crate::engine::EngineHandle;

/// Shared application state, cheap to clone into handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

pub struct Inner {
    pub config: Arc<Config>,
    pub pool: SqlitePool,
    pub engine: EngineHandle,
}

impl AppState {
    pub fn new(config: Arc<Config>, pool: SqlitePool, engine: EngineHandle) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                pool,
                engine,
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
}
