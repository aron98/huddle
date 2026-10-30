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
//!   hints for when to poll again,
//! * [`Room::drain_layer_switches`] hands out the simulcast layer
//!   decisions made since the last drain, for logs and metrics.

mod alloc;
pub mod error;
mod peer;
mod recorder;
mod room;

pub use alloc::SwitchReason;
pub use error::SfuError;
pub use recorder::{RecordedFrame, RecorderSink};
pub use room::{Room, SfuOutput};

/// Alias for the participant identity used across signaling and the SFU.
pub use huddle_protocol::ParticipantId as PeerId;
/// Bitrates of the simulcast layer allocation (str0m's own type).
pub use str0m::bwe::Bitrate;

use std::time::Duration;

use huddle_protocol::VideoLayer;

/// Room-wide settings of the adaptive simulcast layer allocation
/// (ADR 015).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimulcastConfig {
    /// The egress estimate every peer's bandwidth estimator starts from
    /// (str0m `enable_bwe`). 2 Mbps carries one remote high layer plus
    /// audio, so a two-person call starts on High.
    pub initial_estimate: Bitrate,
    /// Pin every peer's allocation to this estimate instead of the
    /// measured one. **Diagnostics and tests only**: it disables
    /// adaptation to real links (str0m still estimates and paces).
    pub estimate_override: Option<Bitrate>,
}

impl Default for SimulcastConfig {
    fn default() -> Self {
        Self {
            initial_estimate: Bitrate::kbps(2_000),
            estimate_override: None,
        }
    }
}

/// One simulcast layer decision for a receiver (ADR 015, D7): what the
/// allocator switched, why, and the inputs it decided on. Drained via
/// [`Room::drain_layer_switches`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerSwitch {
    pub receiver: PeerId,
    pub origin: PeerId,
    pub from: VideoLayer,
    pub to: VideoLayer,
    pub reason: SwitchReason,
    /// The receiver's estimate as the allocator saw it (override
    /// included); `None` while no estimate has arrived yet.
    pub estimate: Option<Bitrate>,
    /// The camera budget that estimate left after the non-adaptive
    /// reserve (audio, screens, single-layer cameras).
    pub budget: Option<Bitrate>,
}

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
    /// The simulcast layer this peer currently gets forwarded from each
    /// other participant's camera: `(origin, layer)`, one entry per
    /// origin. Exposed via `/metrics` so a layer decision is provable
    /// without depending on sender or receiver timing.
    pub layer_preferences: Vec<(PeerId, VideoLayer)>,
    /// The egress estimate this peer's layer allocation runs on,
    /// override included (ADR 015, D7). `None` until str0m's first
    /// estimate arrives.
    pub egress_estimate: Option<Bitrate>,
    /// The desired bitrate last handed to str0m's probe controller.
    pub egress_desired: Option<Bitrate>,
    /// str0m's `can_probe` from the last estimate; a stuck `false` means
    /// the allocation is frozen (no TWCC negotiated).
    pub bwe_can_probe: Option<bool>,
    /// The current upgrade hold: the allocator's hysteresis state.
    pub layer_upgrade_hold: Duration,
    /// The reason of the last layer decision per origin in
    /// `layer_preferences` (the decision, which may still be waiting for
    /// its keyframe). An origin never switched from its default High
    /// has no entry.
    pub layer_switch_reasons: Vec<(PeerId, SwitchReason)>,
}
