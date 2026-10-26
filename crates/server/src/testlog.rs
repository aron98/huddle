//! Test-only tracing capture (issue #85): a thread-local subscriber
//! that records every event emitted while it is installed, so tests
//! can assert exactly what does — and does not — reach the server
//! log. The magic-link flow uses it to prove that a real mailer's
//! links are never printed.

#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

/// The recorded log output of a [`capture`] install.
#[derive(Clone, Default)]
pub struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl CapturedLogs {
    /// Everything captured so far, as UTF-8.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// A writer that appends into the shared capture buffer.
#[derive(Clone)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedWriter {
    type Writer = SharedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install a capturing subscriber for the rest of this scope: every
/// event on the installing thread is recorded into the returned
/// buffer, and nothing is printed to stdout/stderr. A
/// `#[tokio::test]` runtime (current-thread) polls the test's futures
/// on the installing thread, so events from the code under test are
/// captured too.
pub fn capture() -> (CapturedLogs, tracing::subscriber::DefaultGuard) {
    let captured = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(SharedWriter(captured.0.clone()))
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (captured, guard)
}
