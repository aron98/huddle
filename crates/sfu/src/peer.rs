use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{
    Direction, KeyframeRequest, KeyframeRequestKind, MediaData, MediaKind, Mid, Rid,
};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::PeerId;
use crate::error::SfuError;

/// Minimum interval between automatic keyframe requests for the same track.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_secs(1);

/// A media track flowing *into* the SFU from its origin peer.
///
/// The origin peer's [`Peer`] holds the strong reference; every other peer
/// holds a [`Weak`] in its `tracks_out`.
pub(crate) struct TrackIn {
    pub origin: PeerId,
    pub mid: Mid,
    pub kind: MediaKind,
}

/// Bookkeeping for an incoming track: the strong handle plus keyframe
/// request throttling state.
pub(crate) struct TrackInEntry {
    pub track: Arc<TrackIn>,
    pub last_keyframe_request: Option<Instant>,
}

/// State machine of an outgoing track (a `TrackIn` of *another* peer that
/// this peer should receive). Tracks move forward `ToOpen -> Negotiating ->
/// Open`; when the origin disappears they wind down through
/// `ToStop -> NegotiatingStop` and are dropped once the stop is answered.
pub(crate) enum TrackOutState {
    /// Needs an SDP m-line allocated in the next renegotiation.
    ToOpen,
    /// An offer including this track has been sent; waiting for the answer.
    Negotiating(Mid),
    /// Fully negotiated; media can be written to `Mid`.
    Open(Mid),
    /// The origin went away; the m-line is stopped in the next
    /// renegotiation. No media is forwarded from here on.
    ToStop(Mid),
    /// A stop offer has been sent; waiting for the answer to drop the
    /// entry (a stopped m-line can never be reactivated).
    NegotiatingStop(Mid),
}

/// An outgoing track entry on the receiving peer.
pub(crate) struct TrackOut {
    pub origin: PeerId,
    pub mid_in: Mid,
    pub kind: MediaKind,
    pub track_in: Weak<TrackIn>,
    pub state: TrackOutState,
}

/// One polled step of a peer, with all borrowed data owned.
///
/// `MediaData` is moved by value — one allocation per media event is
/// deliberately avoided on this hot path despite the enum size spread.
#[allow(clippy::large_enum_variant)]
pub(crate) enum PollOutcome {
    /// A datagram ready to send to the peer.
    Transmit {
        destination: SocketAddr,
        contents: Vec<u8>,
    },
    /// The peer should be polled again no later than this instant.
    Timeout(Instant),
    /// ICE + DTLS finished for this peer.
    Connected,
    /// The peer is dead (disconnected); it will be removed by the room.
    Gone,
    /// The peer announced a new incoming track (from a remote offer).
    TrackOpened(Arc<TrackIn>),
    /// The peer received media that should be routed to other peers.
    Media(MediaData),
    /// The peer (as a receiver) asked another peer's track for a keyframe.
    KeyframeRequested(KeyframeRequest),
    /// A str0m event we do not act on.
    Ignored,
}

/// One participant's `Rtc` plus the routing bookkeeping around it.
pub(crate) struct Peer {
    pub id: PeerId,
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    tracks_in: Vec<TrackInEntry>,
    tracks_out: Vec<TrackOut>,
    /// True once the client completed its own first offer/answer.
    ///
    /// Renegotiation offers must not be sent before that: the client's
    /// initial offer invalidates any pending server offer, and a stale
    /// offer delivered across that boundary no longer matches the m-line
    /// order the client established, so browsers reject it.
    has_session: bool,
    /// Mute state applied at the SFU: frames of a muted kind arriving
    /// from this peer are not forwarded to anyone.
    audio_muted: bool,
    video_muted: bool,
    pub rx_frames: u64,
    pub tx_frames: u64,
    /// Keyframe (FIR/PLI) requests successfully enqueued toward this
    /// peer. The wire form is SRTCP, invisible to outside observers, so
    /// tests assert on this counter instead of sniffing datagrams.
    pub keyframes_requested: u64,
}

impl Peer {
    pub(crate) fn new(id: PeerId, media_addr: SocketAddr) -> Result<Peer, SfuError> {
        // The server is the ICE-lite side: it never gathers, it just
        // advertises its single media address in every SDP it produces.
        let rtc = Rtc::builder()
            .set_ice_lite(true)
            .clear_codecs()
            .enable_opus(true, false)
            .enable_vp8(true)
            .build(Instant::now());

        let mut peer = Peer {
            id,
            rtc,
            pending: None,
            tracks_in: Vec::new(),
            tracks_out: Vec::new(),
            has_session: false,
            audio_muted: false,
            video_muted: false,
            rx_frames: 0,
            tx_frames: 0,
            keyframes_requested: 0,
        };

        let candidate = Candidate::host(media_addr, "udp")
            .map_err(|e| SfuError::InvalidCandidate(e.to_string()))?;
        peer.rtc.add_local_candidate(candidate);
        Ok(peer)
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    pub(crate) fn disconnect(&mut self) {
        self.rtc.disconnect();
    }

    /// Accept an SDP offer from the client and produce the answer.
    ///
    /// `accept_offer` invalidates any pending offer we sent, so tracks that
    /// were mid-negotiation are reset to `ToOpen` (or back to `ToStop` for
    /// in-flight stops) and renegotiated on the next poll.
    pub(crate) fn answer_sdp_offer(&mut self, sdp: &str) -> Result<String, SfuError> {
        let offer = SdpOffer::from_sdp_string(sdp)?;
        let answer = self.rtc.sdp_api().accept_offer(offer)?;
        self.pending = None;
        self.has_session = true;
        for out in &mut self.tracks_out {
            match out.state {
                TrackOutState::Negotiating(_) => out.state = TrackOutState::ToOpen,
                TrackOutState::NegotiatingStop(mid) => out.state = TrackOutState::ToStop(mid),
                _ => {}
            }
        }
        Ok(answer.to_sdp_string())
    }

    /// Accept the client's answer to a renegotiation offer we sent.
    ///
    /// Returns `(origin, incoming mid)` of every track that just went
    /// live, so the room can fan keyframe requests out to the sources.
    pub(crate) fn accept_sdp_answer(&mut self, sdp: &str) -> Result<Vec<(PeerId, Mid)>, SfuError> {
        let Some(pending) = self.pending.take() else {
            return Err(SfuError::NoPendingOffer);
        };
        let answer = SdpAnswer::from_sdp_string(sdp)?;
        self.rtc.sdp_api().accept_answer(pending, answer)?;
        // Stops are confirmed by the answer: the m-line is dead and the
        // entry can go. Adds become live; if their origin vanished while
        // the offer was in flight, wind them straight back down.
        self.tracks_out
            .retain(|out| !matches!(out.state, TrackOutState::NegotiatingStop(_)));
        let mut opened = Vec::new();
        for out in &mut self.tracks_out {
            if let TrackOutState::Negotiating(mid) = out.state
                && let Some(track) = out.track_in.upgrade()
            {
                out.state = TrackOutState::Open(mid);
                opened.push((track.origin, track.mid));
            } else if let TrackOutState::Negotiating(mid) = out.state {
                out.state = TrackOutState::ToStop(mid);
            }
        }
        Ok(opened)
    }

    /// Add a trickled ICE candidate from the client.
    pub(crate) fn add_remote_candidate(&mut self, candidate: &str) -> Result<(), SfuError> {
        let candidate = Candidate::from_sdp_string(candidate)
            .map_err(|e| SfuError::InvalidCandidate(e.to_string()))?;
        self.rtc.add_remote_candidate(candidate);
        Ok(())
    }

    /// Feed a network input (demultiplexed by the room) into the `Rtc`.
    pub(crate) fn handle_input(&mut self, input: Input) -> Result<(), SfuError> {
        Ok(self.rtc.handle_input(input)?)
    }

    /// Feed a timestamp in, advancing str0m's internal state machines.
    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        let _ = self.rtc.handle_input(Input::Timeout(now));
    }

    pub(crate) fn accepts(&self, input: &Input) -> bool {
        self.rtc.accepts(input)
    }

    /// Number of forwardable outgoing track entries (any state).
    pub(crate) fn tracks_out_count(&self) -> usize {
        self.tracks_out.len()
    }

    /// Number of incoming tracks.
    pub(crate) fn tracks_in_count(&self) -> usize {
        self.tracks_in.len()
    }

    /// All incoming tracks of this peer, for replaying to a newcomer.
    pub(crate) fn tracks_in(&self) -> impl Iterator<Item = &Arc<TrackIn>> {
        self.tracks_in.iter().map(|e| &e.track)
    }

    /// Register another peer's incoming track as an outgoing track here.
    pub(crate) fn open_track(&mut self, track_in: &Arc<TrackIn>) {
        self.tracks_out.push(TrackOut {
            origin: track_in.origin,
            mid_in: track_in.mid,
            kind: track_in.kind,
            track_in: Arc::downgrade(track_in),
            state: TrackOutState::ToOpen,
        });
    }

    /// Update the SFU-side mute state; `None` leaves that kind untouched.
    ///
    /// When video is unmuted, every video source of this peer is asked
    /// for a keyframe (throttled): subscribers were starved of the GOP
    /// while muted, and without a fresh intra frame their tiles would
    /// stay stale until the next PLI.
    pub(crate) fn set_mute(&mut self, audio_muted: Option<bool>, video_muted: Option<bool>) {
        let video_was_muted = self.video_muted;
        if let Some(a) = audio_muted {
            self.audio_muted = a;
        }
        if let Some(v) = video_muted {
            self.video_muted = v;
        }
        if video_was_muted && !self.video_muted {
            let video_mids: Vec<Mid> = self
                .tracks_in
                .iter()
                .filter(|e| matches!(e.track.kind, MediaKind::Video))
                .map(|e| e.track.mid)
                .collect();
            for mid in video_mids {
                self.request_keyframe_throttled(mid, None, KeyframeRequestKind::Fir);
            }
        }
    }

    /// Whether an incoming frame on `mid` may be forwarded right now.
    ///
    /// Unknown m-lines stay forwardable; a known track is dropped while
    /// its kind is muted at the source.
    pub(crate) fn is_forwardable(&self, mid: Mid) -> bool {
        let Some(entry) = self.tracks_in.iter().find(|e| e.track.mid == mid) else {
            return true;
        };
        match entry.track.kind {
            MediaKind::Audio => !self.audio_muted,
            MediaKind::Video => !self.video_muted,
        }
    }

    /// If any tracks are waiting to be opened or stopped, bundle them into
    /// a single renegotiation offer and return its SDP string.
    pub(crate) fn negotiate_if_needed(&mut self) -> Option<String> {
        if !self.has_session || self.pending.is_some() {
            return None;
        }

        let mut changes = self.rtc.sdp_api();
        for out in &mut self.tracks_out {
            match out.state {
                TrackOutState::ToOpen => {
                    let mid = changes.add_media(
                        out.kind,
                        Direction::SendOnly,
                        Some(out.origin.to_string()),
                        None,
                        None,
                    );
                    out.state = TrackOutState::Negotiating(mid);
                }
                TrackOutState::ToStop(mid) => {
                    changes.stop_media(mid);
                    out.state = TrackOutState::NegotiatingStop(mid);
                }
                _ => {}
            }
        }

        let (offer, pending) = changes.apply()?;
        self.pending = Some(pending);
        Some(offer.to_sdp_string())
    }

    /// Wind down entries whose origin track is gone (its peer left).
    ///
    /// Un-negotiated entries are dropped outright; negotiated ones are
    /// stopped in the next renegotiation so the browser's m-line goes
    /// away with the departed participant.
    pub(crate) fn reap_dead_tracks(&mut self) {
        self.tracks_out.retain_mut(|out| {
            if out.track_in.upgrade().is_some() {
                return true;
            }
            match out.state {
                TrackOutState::ToOpen => false,
                TrackOutState::Open(mid) => {
                    out.state = TrackOutState::ToStop(mid);
                    true
                }
                // Already stopping; the answer completes the removal.
                TrackOutState::ToStop(_) | TrackOutState::NegotiatingStop(_) => true,
                // Mid-add with a dead origin: let the answer land, the
                // post-answer sweep turns it into a stop.
                TrackOutState::Negotiating(_) => true,
            }
        });
    }

    /// Poll one step of the underlying `Rtc` and translate it into an
    /// owned [`PollOutcome`]. Errors disconnect the peer.
    pub(crate) fn poll_once(&mut self) -> Result<PollOutcome, SfuError> {
        let output = match self.rtc.poll_output() {
            Ok(output) => output,
            Err(e) => {
                self.rtc.disconnect();
                return Err(e.into());
            }
        };

        Ok(match output {
            Output::Timeout(t) => PollOutcome::Timeout(t),
            Output::Transmit(t) => PollOutcome::Transmit {
                destination: t.destination,
                contents: t.contents.to_vec(),
            },
            Output::Event(e) => self.handle_event(e),
        })
    }

    fn handle_event(&mut self, event: Event) -> PollOutcome {
        match event {
            Event::Connected => PollOutcome::Connected,
            Event::IceConnectionStateChange(state) => {
                if state == IceConnectionState::Disconnected {
                    // This implementation treats a lost ICE connection as
                    // fatal: the signaling layer will notice the peer's
                    // removal and clean up.
                    self.rtc.disconnect();
                    PollOutcome::Gone
                } else {
                    PollOutcome::Ignored
                }
            }
            Event::MediaAdded(m) => {
                let track = Arc::new(TrackIn {
                    origin: self.id,
                    mid: m.mid,
                    kind: m.kind,
                });
                self.tracks_in.push(TrackInEntry {
                    track: Arc::clone(&track),
                    last_keyframe_request: None,
                });
                PollOutcome::TrackOpened(track)
            }
            Event::MediaData(data) => PollOutcome::Media(data),
            Event::KeyframeRequest(req) => PollOutcome::KeyframeRequested(req),
            _ => PollOutcome::Ignored,
        }
    }

    /// The outgoing `Mid` this peer uses for `origin`'s track `mid_in`,
    /// once the track is fully negotiated.
    pub(crate) fn outgoing_mid_for(&self, origin: PeerId, mid_in: Mid) -> Option<Mid> {
        self.tracks_out.iter().find_map(|t| match t.state {
            TrackOutState::Open(mid) if t.origin == origin && t.mid_in == mid_in => Some(mid),
            _ => None,
        })
    }

    /// Look up the origin track behind an outgoing `Mid` (used to route
    /// keyframe requests back to the media source).
    pub(crate) fn track_in_for_outgoing(&self, mid_out: Mid) -> Option<(PeerId, Mid)> {
        self.tracks_out
            .iter()
            .find(|t| matches!(t.state, TrackOutState::Open(m) if m == mid_out))
            .and_then(|t| t.track_in.upgrade())
            .map(|t| (t.origin, t.mid))
    }

    /// Forward one media frame to this peer over the outgoing `mid`.
    ///
    /// Silently drops the frame if the writer or a matching payload type is
    /// not (yet) available; a write error is propagated so the room can
    /// disconnect the peer.
    pub(crate) fn forward_media(&mut self, mid_out: Mid, data: &MediaData) -> Result<(), SfuError> {
        let Some(writer) = self.rtc.writer(mid_out) else {
            return Ok(());
        };
        let Some(pt) = writer.match_params(data.params) else {
            return Ok(());
        };
        writer.write(pt, data.network_time, data.time, data.data.clone())?;
        self.tx_frames += 1;
        Ok(())
    }

    /// Ask the source of `mid` (an incoming track on this peer) for a
    /// keyframe, at most once per interval.
    pub(crate) fn request_keyframe_throttled(
        &mut self,
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) {
        let now = Instant::now();
        let Some(entry) = self.tracks_in.iter_mut().find(|e| e.track.mid == mid) else {
            return;
        };
        let due = entry
            .last_keyframe_request
            .is_none_or(|t| now.duration_since(t) >= KEYFRAME_REQUEST_INTERVAL);
        if !due {
            return;
        }
        entry.last_keyframe_request = Some(now);
        if let Some(mut writer) = self.rtc.writer(mid) {
            match writer.request_keyframe(rid, kind) {
                Ok(()) => self.keyframes_requested += 1,
                Err(e) => tracing::warn!("keyframe request on {mid:?} failed: {e}"),
            }
        }
    }

    /// Relay a receiver's keyframe request to this (origin) peer's
    /// incoming track.
    pub(crate) fn request_keyframe(
        &mut self,
        mid_in: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) -> Result<(), SfuError> {
        let mut writer = self
            .rtc
            .writer(mid_in)
            .ok_or_else(|| SfuError::Internal(format!("no writer for incoming mid {mid_in:?}")))?;
        writer.request_keyframe(rid, kind)?;
        self.keyframes_requested += 1;
        Ok(())
    }
}
