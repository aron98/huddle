use str0m::RtcError;
use str0m::error::SdpError;
use thiserror::Error as ThisError;

/// Errors surfaced by the SFU room API.
#[derive(Debug, ThisError)]
pub enum SfuError {
    /// No peer with the given id exists in the room.
    #[error("no such peer in room")]
    UnknownPeer,
    /// A peer with the given id already exists in the room.
    #[error("peer already exists in room")]
    PeerExists,
    /// The peer has no pending offer that the given SDP could answer.
    #[error("no pending sdp offer for peer")]
    NoPendingOffer,
    /// An SDP document failed to parse or apply.
    #[error("invalid sdp: {0}")]
    InvalidSdp(#[from] SdpError),
    /// An ICE candidate string failed to parse.
    #[error("invalid ice candidate: {0}")]
    InvalidCandidate(String),
    /// An unexpected internal inconsistency (str0m disagreed with our
    /// bookkeeping). Peers are disconnected rather than propagated.
    #[error("internal sfu error: {0}")]
    Internal(String),
    /// An error from the underlying `str0m` [`Rtc`](str0m::Rtc).
    #[error("rtc error: {0}")]
    Rtc(#[from] RtcError),
}
