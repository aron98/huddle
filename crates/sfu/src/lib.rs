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
mod recorder;
mod room;

pub use error::SfuError;
pub use recorder::{RecordedFrame, RecorderSink};
pub use room::{Room, SfuOutput};

/// Alias for the participant identity used across signaling and the SFU.
pub use huddle_protocol::ParticipantId as PeerId;

use huddle_protocol::VideoLayer;

/// Per-peer counters and link stats, exposed for tests and `/metrics`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerStats {
    /// Media frames received from this peer.
    pub rx_frames: u64,
    /// Media frames sent to this peer.
    pub tx_frames: u64,
    /// Media frames shed toward this peer because its str0m egress
    /// queue overflowed (`WriteWithoutPoll` backpressure). The frames
    /// were dropped by str0m before hitting the wire; the peer stays
    /// connected.
    pub dropped_frames: u64,
    /// Keyframe (FIR/PLI) requests enqueued toward this peer. New
    /// subscribers trigger one per source; the SRTCP wire form is
    /// encrypted, so tests assert on this counter instead.
    pub keyframes_requested: u64,
    /// Media payload bytes received from this peer (RTP payload only),
    /// from the latest str0m stats snapshot. Zero until one lands.
    pub bytes_rx: u64,
    /// Media payload bytes sent to this peer (RTP payload only).
    pub bytes_tx: u64,
    /// Ingress bitrate over the last stats window, bits per second.
    pub bitrate_in_bps: u64,
    /// Egress bitrate over the last stats window, bits per second.
    pub bitrate_out_bps: u64,
    /// Most recent round-trip time to this peer, milliseconds.
    pub rtt_ms: Option<u64>,
    /// Packet loss on the wire from this peer (fraction 0.0–1.0).
    pub ingress_loss_fraction: Option<f32>,
    /// Packet loss on the wire toward this peer (fraction 0.0–1.0).
    pub egress_loss_fraction: Option<f32>,
    /// The simulcast layer this peer currently receives from each
    /// other participant's camera: `(origin, layer)`, one entry per
    /// origin. Exposed via `/metrics` so a layer switch is provable
    /// without depending on sender or receiver timing.
    pub layer_preferences: Vec<(PeerId, VideoLayer)>,
}
