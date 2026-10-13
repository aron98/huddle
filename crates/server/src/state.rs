use std::sync::Arc;

use crate::config::Config;

/// Shared application state, cheap to clone into handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

pub struct Inner {
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            inner: Arc::new(Inner { config }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }
}
