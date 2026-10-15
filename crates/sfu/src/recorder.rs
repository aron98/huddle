//! The recording tap: frames as they pass the forwarding gate.
//!
//! A [`RecorderSink`] is installed on a [`Room`](crate::Room) by the
//! embedder (the server's recording lifecycle). The room calls it for
//! every frame that survives the mute/share gate — i.e. exactly the media
//! other participants receive — so guests are recorded as they see it.

use std::sync::Arc;

use str0m::media::MediaKind;

use crate::PeerId;

/// One media frame captured by the recording tap.
///
/// The payload is str0m's *depayloadized* frame: the complete codec
/// bitstream unit (VP8 frame / Opus packet), with RTP packetization
/// already stripped. Sinks that want plain RTP on disk must synthesize
/// the headers themselves (`pt`, `seq`, `timestamp` are provided).
#[derive(Debug)]
pub struct RecordedFrame {
    /// The participant whose track produced this frame.
    pub origin: PeerId,
    /// Audio or video.
    pub kind: MediaKind,
    /// True for screen-share tracks (vs the origin's camera).
    pub screen: bool,
    /// The negotiated RTP payload type, for muxer manifests.
    pub pt: u8,
    /// The RTP sequence number of the frame's first packet (u16 form).
    pub seq: u16,
    /// The raw RTP timestamp (in the track's clock rate: 48 kHz audio,
    /// 90 kHz video).
    pub timestamp: u64,
    /// Whether this frame follows the previous one contiguously.
    pub contiguous: bool,
    /// Whether the frame is a keyframe (video only).
    pub keyframe: bool,
    /// The complete depayloaded frame (codec bitstream, no RTP headers).
    pub payload: Arc<[u8]>,
}

/// Where recorded frames go.
///
/// Installed per meeting via
/// [`Room::set_recorder`](crate::Room::set_recorder). **Fail-open by
/// contract**: the room's media path never depends on the sink. The
/// implementation must be cheap and non-blocking (the SFU's poll loop
/// calls it inline); a panicked or wedged recorder must never stall or
/// crash the meeting — the embedder achieves this by handing frames to
/// a writer over a bounded channel and dropping them when it is full.
pub trait RecorderSink: Send + Sync {
    /// Receive one frame. Never panics, never blocks.
    fn record(&self, frame: RecordedFrame);
}
