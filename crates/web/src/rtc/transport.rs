//! The signaling transport boundary.
//!
//! `SignalingStream` abstracts the WebSocket so the driver in
//! [`super::RtcClient`] runs identically in the browser (backed by
//! `web_sys::WebSocket`) and in native tests (backed by a real socket against
//! the mock server).

use huddle_protocol::{ClientMessage, ServerMessage};

/// Failures on the signaling connection.
#[derive(Debug, thiserror::Error)]
pub enum SignalingError {
    /// The underlying socket failed or was closed.
    #[error("signaling connection failed: {0}")]
    Socket(String),
    /// A message could not be (de)serialized.
    #[error("malformed signaling message: {0}")]
    Malformed(String),
}

/// A message-oriented signaling connection: JSON `ClientMessage`s out,
/// `ServerMessage`s in.
pub trait SignalingStream {
    /// Send one message to the server.
    async fn send(&mut self, msg: ClientMessage) -> Result<(), SignalingError>;

    /// Receive the next server message. `None` means the connection is closed
    /// and no further messages will arrive.
    async fn next_message(&mut self) -> Option<Result<ServerMessage, SignalingError>>;
}
