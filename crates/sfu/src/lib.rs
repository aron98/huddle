//! The huddle media plane (SFU).
//!
//! Sans-io by design: the entire crate is built on `str0m` and is
//! unit-testable without sockets — two `Rtc` instances can be wired together
//! in-process (see `docs/adr/001-sfu-on-str0m.md`).
//!
//! The central type is [`Room`]: it owns one peer per
//! participant, answers their SDP offers, and forwards media between them.
//! The embedder (the server) is responsible for the actual sockets:
//!
//! * incoming datagrams are demultiplexed with [`Room::accepts`] and fed in
//!   via [`Room::handle_input`],
//! * [`Room::poll_output`] yields [`SfuOutput`]s — transmits to send,
//!   renegotiation offers to relay over the signaling channel, and timer
//!   hints for when to poll again.

pub mod error;
mod peer;
mod room;

pub use error::SfuError;
pub use room::{Room, SfuOutput};

/// Alias for the participant identity used across signaling and the SFU.
pub use huddle_protocol::ParticipantId as PeerId;

/// Basic per-peer counters exposed for tests and the `/stats` endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerStats {
    /// Media frames received from this peer.
    pub rx_frames: u64,
    /// Media frames sent to this peer.
    pub tx_frames: u64,
}
