//! The peer-connection boundary.
//!
//! `PeerConnection` abstracts the browser's `RTCPeerConnection` so the
//! signaling driver can be exercised natively against a fake implementation
//! (see `tests/mock_server.rs`).

/// A locally gathered ICE candidate, ready to trickle to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCandidate {
    /// The SDP `candidate:` string (empty marks end-of-candidates).
    pub candidate: String,
    pub sdp_mid: Option<String>,
    pub sdp_m_line_index: Option<u16>,
}

impl From<LocalCandidate> for huddle_protocol::ClientMessage {
    fn from(c: LocalCandidate) -> Self {
        huddle_protocol::ClientMessage::IceCandidate {
            candidate: c.candidate,
            sdp_mid: c.sdp_mid,
            sdp_m_line_index: c.sdp_m_line_index,
        }
    }
}

/// Failures from the underlying peer connection.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("peer connection failure: {0}")]
    Failed(String),
}

/// The WebRTC side of the client: SDP offers/answers and ICE, independent of
/// how signaling is transported.
pub trait PeerConnection {
    /// Create an SDP offer reflecting the currently attached local media.
    async fn create_offer(&mut self) -> Result<String, PeerError>;

    /// Apply the server's answer to a locally created offer.
    async fn apply_answer(&mut self, sdp: String) -> Result<(), PeerError>;

    /// Apply a server-initiated offer (renegotiation) and produce the local
    /// answer to send back.
    async fn apply_remote_offer(&mut self, sdp: String) -> Result<String, PeerError>;

    /// Apply one trickled remote candidate.
    async fn add_remote_candidate(&mut self, candidate: LocalCandidate) -> Result<(), PeerError>;

    /// Drain locally gathered candidates so they can be trickled out.
    fn take_local_candidates(&mut self) -> Vec<LocalCandidate>;

    /// Attach the staged screen track so the next offer announces it.
    ///
    /// How a track comes to be "staged" is implementation-specific (the
    /// browser keeps a handle for its `getDisplayMedia` flow); the driver
    /// only needs to know the attachment happened before it renegotiates.
    async fn add_screen_track(&mut self) -> Result<(), PeerError> {
        Err(PeerError::Failed(
            "screen share is not supported".to_owned(),
        ))
    }

    /// Detach the track attached by [`PeerConnection::add_screen_track`],
    /// so the next offer withdraws it.
    async fn remove_screen_track(&mut self) -> Result<(), PeerError> {
        Err(PeerError::Failed(
            "screen share is not supported".to_owned(),
        ))
    }
}
