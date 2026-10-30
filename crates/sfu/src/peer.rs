use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use huddle_protocol::VideoLayer;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{
    Direction, KeyframeRequest, KeyframeRequestKind, MediaData, MediaKind, Mid, Rid,
};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcError};

use crate::alloc::{LayerAllocator, Source};
use crate::error::SfuError;
use crate::{PeerId, SimulcastConfig};
/// Minimum interval between automatic keyframe requests for the same track.
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_secs(1);

/// How long a pending layer switch waits for a keyframe on the target
/// rid before cutting over anyway (ADR 015, D6).
const LAYER_SWITCH_TIMEOUT: Duration = Duration::from_secs(1);

/// A simulcast rid counts as live while its last frame is at most this
/// old (ADR 015, D2): a sender under uplink pressure stops encoding its
/// top layer, and receivers must not stay parked on it.
const RID_LIVENESS: Duration = Duration::from_millis(1500);

/// A media track flowing *into* the SFU from its origin peer.
///
/// The origin peer's [`Peer`] holds the strong reference; every other peer
/// holds a [`Weak`] in its `tracks_out`.
pub(crate) struct TrackIn {
    pub origin: PeerId,
    pub mid: Mid,
    pub kind: MediaKind,
    /// True for screen-share tracks. They are labeled with an
    /// `"{origin}-screen"` msid and gated by the share state rather than
    /// the camera mute.
    pub screen: bool,
    /// Negotiated simulcast rids for this track, in the sender's
    /// encoding order (highest quality first). Empty for audio, screen
    /// shares and non-simulcast cameras — those are forwarded as-is.
    pub rids: Vec<Rid>,
    /// Screen tracks stop and resume without their peer dying (the
    /// sharer keeps the connection and simply stops sending). `active`
    /// gates forwarding and newcomer replay; camera/mic tracks are always
    /// active while the peer lives.
    active: AtomicBool,
}

impl TrackIn {
    fn new(origin: PeerId, mid: Mid, kind: MediaKind, screen: bool, rids: Vec<Rid>) -> Self {
        Self {
            origin,
            mid,
            kind,
            screen,
            rids,
            active: AtomicBool::new(true),
        }
    }

    /// Whether this track currently delivers media.
    pub(crate) fn active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }
}

/// The rid a receiver set on `layer` should get from a track with `rids`
/// (highest quality first). `None` means the track is not simulcast:
/// everything is forwarded.
pub(crate) fn chosen_rid(rids: &[Rid], layer: VideoLayer) -> Option<Rid> {
    if rids.is_empty() {
        return None;
    }
    match layer {
        VideoLayer::High => rids.first().copied(),
        VideoLayer::Low => rids.last().copied(),
    }
}

/// Bookkeeping for an incoming track: the strong handle plus keyframe
/// request throttling state, per simulcast rid (`None` = whole track).
pub(crate) struct TrackInEntry {
    pub track: Arc<TrackIn>,
    pub last_keyframe_request: HashMap<Option<Rid>, Instant>,
    /// Arrival time of the latest frame per rid: simulcast liveness.
    pub last_frame: HashMap<Option<Rid>, Instant>,
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
    /// Copied from the source [`TrackIn`]: decides the msid the SFU
    /// labels this m-line with (plain origin id vs `"{origin}-screen"`).
    pub screen: bool,
    /// Creation order on this receiver: the layer allocator's last
    /// tiebreaker (earlier subscriptions keep High first).
    pub seq: u64,
    /// The simulcast layer currently forwarded from `origin`'s camera
    /// (screens stay single-layer and are never filtered).
    pub layer: VideoLayer,
    /// A switch waiting for a keyframe on the target layer's rid, and
    /// when it was asked for. Until that keyframe arrives (or
    /// [`LAYER_SWITCH_TIMEOUT`] passes) the current layer keeps flowing,
    /// so the receiver never decodes delta frames against the wrong
    /// reference (ADR 015, D6).
    pub pending: Option<(VideoLayer, Instant)>,
    pub track_in: Weak<TrackIn>,
    pub state: TrackOutState,
}

/// A screen-share state change observed in a client offer.
pub(crate) struct ScreenFlip {
    pub track: Arc<TrackIn>,
    pub active: bool,
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
    /// Simulcast rids per incoming m-line, recorded from the client's
    /// offers before `MediaAdded` fires (the event carries no rids, so
    /// `TrackIn` creation reads them from here).
    rids_by_mid: HashMap<Mid, Vec<Rid>>,
    /// Set when the client announces a screen share
    /// ([`crate::Room::set_sharing`]); the next remote video m-line is
    /// marked as the screen track. `false` is also set when a share
    /// stops, so a late offer never creates a phantom screen.
    screen_pending: bool,
    /// Companion to `screen_pending` for share audio: the next remote
    /// audio m-line armed while a share is pending is the screen's
    /// sound (the mic m-line is negotiated in the initial offer, before
    /// any share can be announced). Cleared when the share stops.
    screen_audio_pending: bool,
    pub rx_frames: u64,
    pub tx_frames: u64,
    /// Media frames shed because this peer's str0m egress queue
    /// overflowed ([`RtcError::WriteWithoutPoll`] backpressure). str0m
    /// drops the frame itself; the peer stays connected.
    pub dropped_frames: u64,
    /// Keyframe (FIR/PLI) requests successfully enqueued toward this
    /// peer. The wire form is SRTCP, invisible to outside observers, so
    /// tests assert on this counter instead of sniffing datagrams.
    pub keyframes_requested: u64,
    /// Link stats from the latest str0m snapshot (~1s once connected).
    stats: Option<LinkStats>,
    /// Turns this receiver's egress bandwidth estimate into simulcast
    /// layers (ADR 015).
    alloc: LayerAllocator,
    /// The desired bitrate last handed to str0m's probe controller.
    desired: Option<Bitrate>,
    /// Next [`TrackOut::seq`].
    next_out_seq: u64,
    /// Measured ingress per incoming `(mid, rid)`, from str0m's periodic
    /// `MediaIngressStats`: `(bytes, at, bitrate_bps)`. Screen shares
    /// are reserved at this rate in receivers' allocations.
    ingress: HashMap<(Mid, Option<Rid>), (u64, Instant, u64)>,
    /// `(bytes_rx, bytes_tx, timestamp)` of the previous snapshot,
    /// kept to turn byte counters into per-window bitrates.
    stats_prev: Option<(u64, u64, Instant)>,
}

/// Link-level stats captured from str0m's periodic `PeerStats` events.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LinkStats {
    pub(crate) bytes_rx: u64,
    pub(crate) bytes_tx: u64,
    pub(crate) bitrate_in_bps: u64,
    pub(crate) bitrate_out_bps: u64,
    pub(crate) rtt_ms: Option<u64>,
    pub(crate) ingress_loss_fraction: Option<f32>,
    pub(crate) egress_loss_fraction: Option<f32>,
}

impl Peer {
    pub(crate) fn new(
        id: PeerId,
        media_addr: SocketAddr,
        simulcast: &SimulcastConfig,
    ) -> Result<Peer, SfuError> {
        // The server is the ICE-lite side: it never gathers, it just
        // advertises its single media address in every SDP it produces.
        let mut config = Rtc::builder()
            .set_ice_lite(true)
            .clear_codecs()
            .enable_opus(true, false)
            .enable_vp8(true)
            // Periodic stats events (bytes, loss, RTT) feed /metrics.
            .set_stats_interval(Some(Duration::from_secs(1)))
            // TWCC-driven egress bandwidth estimation (ADR 015): drives
            // the per-receiver simulcast layers. Also turns on str0m's
            // pacer (1.1 × the estimate).
            .enable_bwe(Some(simulcast.initial_estimate));
        // transport-cc on video only. str0m sends its BWE probe padding
        // on the first m-line that negotiated transport-cc, which would
        // be the mic: Chrome surfaces padding-only audio packets as
        // empty encoded frames, which the E2EE transform then counts as
        // decrypt failures (libwebrtc itself never pads audio). Without
        // it on Opus, probes ride a forwarded video m-line instead.
        for params in config.codec_config().iter_mut() {
            if params.spec().codec.is_audio() {
                params.set_fb_transport_cc(false);
            }
        }
        let rtc = config.build(Instant::now());
        let mut alloc = LayerAllocator::default();
        alloc.set_override(simulcast.estimate_override);

        let mut peer = Peer {
            id,
            rtc,
            pending: None,
            tracks_in: Vec::new(),
            tracks_out: Vec::new(),
            has_session: false,
            audio_muted: false,
            video_muted: false,
            rids_by_mid: HashMap::new(),
            screen_pending: false,
            screen_audio_pending: false,
            rx_frames: 0,
            tx_frames: 0,
            dropped_frames: 0,
            keyframes_requested: 0,
            stats: None,
            stats_prev: None,
            alloc,
            desired: None,
            next_out_seq: 0,
            ingress: HashMap::new(),
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
    ///
    /// Returns the answer and every screen track of this peer whose
    /// share state flipped in this offer — the room re-subscribes peers
    /// for resumes (stops converge via the sweep on each negotiation
    /// attempt).
    pub(crate) fn answer_sdp_offer(
        &mut self,
        sdp: &str,
    ) -> Result<(String, Vec<ScreenFlip>), SfuError> {
        let offer = SdpOffer::from_sdp_string(sdp)?;

        // Record simulcast rids before accepting: MediaAdded fires later
        // (on poll) and needs them to tag the incoming track. Screen
        // tracks are never simulcast — leave their mids unrecorded so
        // forwarding stays unconditional.
        for line in &offer.media_lines {
            if !line.rids().is_empty() {
                self.rids_by_mid.insert(line.mid(), line.rids());
            }
        }

        // Screen tracks flip with the direction the client offers:
        // removeTrack turns the m-line recvonly, a re-share (reusing
        // the same transceiver) turns it sendrecv again. The offer is
        // the authoritative statement of what the client will send —
        // str0m's MediaChanged event covers the same transition but
        // its pending flag is unreliable (see `handle_event`).
        let mut screens = Vec::new();
        for line in &offer.media_lines {
            let mid = line.mid();
            let Some(entry) = self.tracks_in.iter().find(|e| e.track.mid == mid) else {
                continue;
            };
            if !entry.track.screen {
                continue;
            }
            let active = line.direction().is_sending();
            if entry.track.active() != active {
                entry.track.set_active(active);
                screens.push(ScreenFlip {
                    track: Arc::clone(&entry.track),
                    active,
                });
            }
        }

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
        Ok((answer.to_sdp_string(), screens))
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
        // A stopped screen share is not (re)played to newcomers: it would
        // open an m-line that never delivers media. A resume goes through
        // [`Peer::open_or_resume_track`], which resets the state directly.
        if track_in.screen && !track_in.active() {
            return;
        }
        let seq = self.next_out_seq;
        self.next_out_seq += 1;
        self.tracks_out.push(TrackOut {
            origin: track_in.origin,
            mid_in: track_in.mid,
            kind: track_in.kind,
            screen: track_in.screen,
            seq,
            // Default High (ADR 015): the allocator downshifts from here.
            layer: VideoLayer::High,
            pending: None,
            track_in: Arc::downgrade(track_in),
            state: TrackOutState::ToOpen,
        });
    }

    /// Open `track_in` here, or wake an existing stopped entry for it
    /// (a screen share resumed). Idempotent: an already-open or
    /// mid-negotiation entry is left alone.
    pub(crate) fn open_or_resume_track(&mut self, track_in: &Arc<TrackIn>) {
        if let Some(out) = self
            .tracks_out
            .iter_mut()
            .find(|o| o.origin == track_in.origin && o.mid_in == track_in.mid)
        {
            if matches!(
                out.state,
                TrackOutState::ToStop(_) | TrackOutState::NegotiatingStop(_)
            ) {
                out.state = TrackOutState::ToOpen;
            }
            return;
        }
        self.open_track(track_in);
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
                self.request_keyframe_all_layers_throttled(mid, KeyframeRequestKind::Fir);
            }
        }
    }

    /// Whether an incoming frame on `mid` may be forwarded right now.
    ///
    /// Unknown m-lines stay forwardable; a known camera/mic track is
    /// dropped while its kind is muted at the source. Screen tracks are
    /// not tied to the camera mute — the share state (`active`) alone
    /// decides whether they flow.
    pub(crate) fn is_forwardable(&self, mid: Mid) -> bool {
        let Some(entry) = self.tracks_in.iter().find(|e| e.track.mid == mid) else {
            return true;
        };
        if entry.track.screen {
            return entry.track.active();
        }
        match entry.track.kind {
            MediaKind::Audio => !self.audio_muted,
            MediaKind::Video => !self.video_muted,
        }
    }

    /// The incoming track on `mid`, if this peer owns one (used by the
    /// recording tap to resolve a frame's track kind and origin).
    pub(crate) fn track_in(&self, mid: Mid) -> Option<&Arc<TrackIn>> {
        self.tracks_in
            .iter()
            .find(|e| e.track.mid == mid)
            .map(|e| &e.track)
    }

    /// If any tracks are waiting to be opened or stopped, bundle them into
    /// a single renegotiation offer and return its SDP string.
    pub(crate) fn negotiate_if_needed(&mut self) -> Option<String> {
        // Screen stops mark entries below before the pending gate: stops
        // queue up even while another offer is in flight, and the sweep
        // runs again on every poll.
        self.sweep_screens();

        if !self.has_session || self.pending.is_some() {
            return None;
        }

        let mut changes = self.rtc.sdp_api();
        for out in &mut self.tracks_out {
            match out.state {
                TrackOutState::ToOpen => {
                    // Screen shares travel on their own labeled stream so
                    // viewers can render (and clean up) them separately
                    // from the sharer's camera tile.
                    let msid = if out.screen {
                        format!("{}-screen", out.origin)
                    } else {
                        out.origin.to_string()
                    };
                    let mid =
                        changes.add_media(out.kind, Direction::SendOnly, Some(msid), None, None);
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

    /// Wind down entries whose source screen track stopped sharing
    /// (the sharer's peer connection lives on, so the weak reference is
    /// still alive — only `active` went false).
    ///
    /// Un-negotiated entries are dropped outright; negotiated ones are
    /// stopped in the next renegotiation. This mirrors
    /// [`Peer::reap_dead_tracks`] for the share-stop case and runs on
    /// every negotiation attempt, so stop state set anywhere converges.
    pub(crate) fn sweep_screens(&mut self) {
        self.tracks_out.retain_mut(|out| {
            let Some(track) = out.track_in.upgrade() else {
                return true; // dead-origin entries are reaped elsewhere
            };
            if !track.screen || track.active() {
                return true;
            }
            match out.state {
                // Never opened: nothing to stop.
                TrackOutState::ToOpen => false,
                TrackOutState::Open(mid) => {
                    out.state = TrackOutState::ToStop(mid);
                    true
                }
                // Mid-flight or already stopping; the answer path resolves.
                _ => true,
            }
        });
    }

    /// Update the share state. `true` arms the next remote video m-line
    /// as the screen track; `false` also deactivates this peer's live
    /// screen tracks (subscribers' entries are swept on their next
    /// negotiation).
    pub(crate) fn set_sharing(&mut self, sharing: bool) {
        self.screen_pending = sharing;
        self.screen_audio_pending = sharing;
        if !sharing {
            for entry in &self.tracks_in {
                if entry.track.screen && entry.track.active() {
                    entry.track.set_active(false);
                }
            }
        }
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
                // The client announces a share before adding the m-line,
                // so the next remote video m-line is the screen — and,
                // when the picked source carries sound, the next audio
                // m-line is the share's audio (the mic is negotiated in
                // the initial offer, before any share). Order is
                // guaranteed: the Sharing message reaches the room
                // before the offer that carries the new m-lines.
                let screen = match m.kind {
                    MediaKind::Video if self.screen_pending => {
                        self.screen_pending = false;
                        true
                    }
                    MediaKind::Audio if self.screen_audio_pending => {
                        self.screen_audio_pending = false;
                        true
                    }
                    _ => false,
                };
                let rids = if screen {
                    Vec::new()
                } else {
                    self.rids_by_mid.get(&m.mid).cloned().unwrap_or_default()
                };
                let track = Arc::new(TrackIn::new(self.id, m.mid, m.kind, screen, rids));
                self.tracks_in.push(TrackInEntry {
                    track: Arc::clone(&track),
                    last_keyframe_request: HashMap::new(),
                    last_frame: HashMap::new(),
                });
                PollOutcome::TrackOpened(track)
            }
            // Screen-share stop/resume is tracked from the client's
            // offers in [`Peer::answer_sdp_offer`] instead of this
            // event: str0m's pending MediaChanged flag can be cleared
            // by its internal direction bookkeeping before we ever
            // poll it, which makes the event unreliable for the
            // stop/reshare transitions (observed with re-shares on a
            // reused transceiver never surfacing the event).
            Event::MediaData(data) => PollOutcome::Media(data),
            Event::KeyframeRequest(req) => PollOutcome::KeyframeRequested(req),
            Event::PeerStats(s) => {
                self.record_peer_stats(s);
                PollOutcome::Ignored
            }
            Event::MediaIngressStats(s) => {
                self.record_ingress(s.mid, s.rid, s.bytes, s.timestamp);
                PollOutcome::Ignored
            }
            // REMB is ignored: browsers send TWCC feedback whenever
            // transport-cc is negotiated, and REMB caps one m-line only.
            Event::EgressBitrateEstimate(BweKind::Twcc {
                estimate,
                can_probe,
            }) => {
                self.alloc.on_estimate(estimate, can_probe);
                PollOutcome::Ignored
            }
            _ => PollOutcome::Ignored,
        }
    }

    /// Capture one str0m stats snapshot, turning the byte counters into
    /// per-window bitrates using the previous snapshot's timestamp.
    fn record_peer_stats(&mut self, s: str0m::stats::PeerStats) {
        let (bitrate_in_bps, bitrate_out_bps) = match self.stats_prev {
            Some((prev_rx, prev_tx, at)) => {
                let elapsed = s.timestamp.saturating_duration_since(at).as_secs_f64();
                if elapsed > 0.0 {
                    (
                        ((s.bytes_rx.saturating_sub(prev_rx)) as f64 * 8.0 / elapsed) as u64,
                        ((s.bytes_tx.saturating_sub(prev_tx)) as f64 * 8.0 / elapsed) as u64,
                    )
                } else {
                    (0, 0)
                }
            }
            None => (0, 0),
        };
        self.stats = Some(LinkStats {
            bytes_rx: s.bytes_rx,
            bytes_tx: s.bytes_tx,
            bitrate_in_bps,
            bitrate_out_bps,
            rtt_ms: s.rtt.map(|d| d.as_millis() as u64),
            ingress_loss_fraction: s.ingress_loss_fraction,
            egress_loss_fraction: s.egress_loss_fraction,
        });
        self.stats_prev = Some((s.bytes_rx, s.bytes_tx, s.timestamp));
    }

    /// Turn one `MediaIngressStats` byte counter into a bitrate.
    fn record_ingress(&mut self, mid: Mid, rid: Option<Rid>, bytes: u64, at: Instant) {
        let bps = match self.ingress.get(&(mid, rid)) {
            Some(&(prev, prev_at, _)) => {
                let elapsed = at.saturating_duration_since(prev_at).as_secs_f64();
                if elapsed > 0.0 {
                    (bytes.saturating_sub(prev) as f64 * 8.0 / elapsed) as u64
                } else {
                    0
                }
            }
            None => 0,
        };
        self.ingress.insert((mid, rid), (bytes, at, bps));
    }

    /// Measured ingress bitrate of incoming `mid` (all rids), once two
    /// stats snapshots have landed.
    pub(crate) fn ingress_bps(&self, mid: Mid) -> Option<u64> {
        let mut rates = self
            .ingress
            .iter()
            .filter(|((m, _), (_, _, bps))| *m == mid && *bps > 0)
            .map(|(_, (_, _, bps))| *bps)
            .peekable();
        rates.peek()?;
        Some(rates.sum())
    }

    /// Record a frame's arrival on incoming `mid`/`rid` (liveness).
    pub(crate) fn note_frame(&mut self, mid: Mid, rid: Option<Rid>, at: Instant) {
        if let Some(entry) = self.tracks_in.iter_mut().find(|e| e.track.mid == mid) {
            entry.last_frame.insert(rid, at);
        }
    }

    /// Whether incoming `mid`'s `rid` delivered a frame recently.
    pub(crate) fn rid_live(&self, mid: Mid, rid: Option<Rid>, now: Instant) -> bool {
        self.tracks_in
            .iter()
            .find(|e| e.track.mid == mid)
            .and_then(|e| e.last_frame.get(&rid))
            .is_some_and(|t| now.saturating_duration_since(*t) <= RID_LIVENESS)
    }

    /// This receiver's live subscriptions (not winding down), with
    /// their creation order: the layer allocator's inputs.
    pub(crate) fn subscriptions(&self) -> Vec<(u64, Arc<TrackIn>)> {
        self.tracks_out
            .iter()
            .filter(|t| {
                !matches!(
                    t.state,
                    TrackOutState::ToStop(_) | TrackOutState::NegotiatingStop(_)
                )
            })
            .filter_map(|t| t.track_in.upgrade().map(|track| (t.seq, track)))
            .collect()
    }

    /// Set this receiver's focused origin (a priority hint, ADR 015 D5).
    pub(crate) fn set_focus(&mut self, focus: Option<PeerId>) {
        self.alloc.set_focus(focus);
    }

    pub(crate) fn focus(&self) -> Option<PeerId> {
        self.alloc.focus()
    }

    /// Pin (or release) the egress estimate the allocation runs on.
    pub(crate) fn set_estimate_override(&mut self, estimate: Option<Bitrate>) {
        self.alloc.set_override(estimate);
    }

    /// Run one layer-allocation round for this receiver and apply its
    /// decisions. Returns the `(origin, mid, rid)` keyframe requests the
    /// room must relay to the origins of switched layers.
    pub(crate) fn reallocate(
        &mut self,
        now: Instant,
        sources: &[Source],
        reserve: Bitrate,
    ) -> Vec<(PeerId, Mid, Rid)> {
        let desired = LayerAllocator::desired_bitrate(sources.len(), reserve);
        if !desired.is_zero() && self.desired != Some(desired) {
            self.rtc.bwe().set_desired_bitrate(desired);
            self.desired = Some(desired);
        }
        let mut keyframes = Vec::new();
        for d in self.alloc.evaluate(now, sources, reserve) {
            tracing::debug!(
                receiver = %self.id,
                origin = %d.origin,
                layer = ?d.layer,
                reason = ?d.reason,
                estimate = ?self.alloc.effective_estimate(),
                "simulcast layer switch"
            );
            // Leaving a layer that no longer delivers frames (a stalled
            // sender, or a subscription with nothing flowing yet): there
            // is nothing to keep forwarding, so switch without the gate.
            let leaving_live =
                sources
                    .iter()
                    .find(|s| s.origin == d.origin)
                    .is_some_and(|s| match d.layer {
                        VideoLayer::High => s.low_live,
                        VideoLayer::Low => s.high_live,
                    });
            let immediate = !leaving_live;
            for (mid, rid) in self.switch_layer(d.origin, d.layer, immediate, now) {
                keyframes.push((d.origin, mid, rid));
            }
        }
        keyframes
    }

    /// The latest link stats snapshot, if one has arrived yet.
    pub(crate) fn link_stats(&self) -> Option<LinkStats> {
        self.stats
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

    /// Whether the frame's simulcast rid matches the layer forwarded on
    /// the outgoing track. Non-simulcast sources (audio, screens, legacy
    /// cameras) always match.
    ///
    /// A pending switch completes here: on the first keyframe of the
    /// target rid, or on any frame once [`LAYER_SWITCH_TIMEOUT`] has
    /// passed without one (frame arrival times keep this sans-io).
    fn wants_layer(&mut self, mid_out: Mid, data: &MediaData) -> bool {
        let Some(out) = self
            .tracks_out
            .iter_mut()
            .find(|t| matches!(t.state, TrackOutState::Open(m) if m == mid_out))
        else {
            return true;
        };
        let Some(track) = out.track_in.upgrade() else {
            return true;
        };
        if let Some((target, since)) = out.pending {
            let target_rid = chosen_rid(&track.rids, target);
            let keyframe = data.rid.is_some() && data.rid == target_rid && data.is_keyframe();
            let timed_out =
                data.network_time.saturating_duration_since(since) >= LAYER_SWITCH_TIMEOUT;
            if keyframe || timed_out {
                out.layer = target;
                out.pending = None;
            }
        }
        let Some(rid) = chosen_rid(&track.rids, out.layer) else {
            return true;
        };
        data.rid == Some(rid)
    }

    /// Forward one media frame to this peer over the outgoing `mid`.
    ///
    /// Simulcast sources are filtered to the receiver's chosen layer.
    /// Silently drops the frame if the writer or a matching payload type is
    /// not (yet) available; a write error is propagated to the room, which
    /// decides the peer's fate via [`Peer::handle_forward_error`].
    pub(crate) fn forward_media(&mut self, mid_out: Mid, data: &MediaData) -> Result<(), SfuError> {
        if !self.wants_layer(mid_out, data) {
            return Ok(());
        }
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

    /// Handle an error from [`Peer::forward_media`].
    ///
    /// `RtcError::WriteWithoutPoll` is egress backpressure: str0m's send
    /// queue already dropped the frame, so this is a load-shed, not a
    /// transport failure — count it and keep the peer connected (a busy
    /// receiver catches up as soon as it is polled again). Any other
    /// write error is treated as fatal and disconnects the peer, as
    /// before.
    pub(crate) fn handle_forward_error(&mut self, e: SfuError) {
        if matches!(e, SfuError::Rtc(RtcError::WriteWithoutPoll)) {
            self.dropped_frames += 1;
            tracing::debug!(
                "peer {} egress queue full, shedding frame (dropped: {})",
                self.id,
                self.dropped_frames
            );
        } else {
            tracing::warn!("forward to peer {} failed, disconnecting: {e}", self.id);
            self.disconnect();
        }
    }

    /// Ask the source of `mid` for a keyframe, at most once per interval
    /// and rid.
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
            .get(&rid)
            .is_none_or(|t| now.duration_since(*t) >= KEYFRAME_REQUEST_INTERVAL);
        if !due {
            return;
        }
        entry.last_keyframe_request.insert(rid, now);
        if let Some(mut writer) = self.rtc.writer(mid) {
            match writer.request_keyframe(rid, kind) {
                Ok(()) => self.keyframes_requested += 1,
                Err(e) => tracing::warn!("keyframe request on {mid:?} failed: {e}"),
            }
        }
    }

    /// Ask the source of `mid` for keyframes on every simulcast layer (or
    /// the whole track when it is not simulcast), each throttled
    /// independently per layer.
    pub(crate) fn request_keyframe_all_layers_throttled(
        &mut self,
        mid: Mid,
        kind: KeyframeRequestKind,
    ) {
        let rids: Vec<Option<Rid>> = match self.tracks_in.iter().find(|e| e.track.mid == mid) {
            Some(e) if !e.track.rids.is_empty() => e.track.rids.iter().copied().map(Some).collect(),
            _ => vec![None],
        };
        for rid in rids {
            self.request_keyframe_throttled(mid, rid, kind);
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

    /// Switch the simulcast layer forwarded from `origin`'s camera
    /// tracks (screens and audio are never filtered). Returns the
    /// `(origin mid, rid)` pairs a keyframe should be requested on for the
    /// newly chosen layer — one per simulcast camera track of `origin`.
    ///
    /// On an open track the switch is keyframe-gated unless `immediate`:
    /// the current layer keeps flowing until the target rid delivers a
    /// keyframe (see [`Peer::wants_layer`]). A track that is not open
    /// yet forwards nothing, so it switches at once.
    fn switch_layer(
        &mut self,
        origin: PeerId,
        layer: VideoLayer,
        immediate: bool,
        now: Instant,
    ) -> Vec<(Mid, Rid)> {
        let mut keyframes = Vec::new();
        for out in self.tracks_out.iter_mut() {
            if out.origin != origin || out.screen || !matches!(out.kind, MediaKind::Video) {
                continue;
            }
            if out.layer == layer {
                // Back to the layer still flowing: drop any pending switch.
                out.pending = None;
                continue;
            }
            if out.pending.is_some_and(|(target, _)| target == layer) {
                continue;
            }
            if matches!(out.state, TrackOutState::Open(_)) && !immediate {
                out.pending = Some((layer, now));
            } else {
                out.layer = layer;
                out.pending = None;
            }
            if let Some(track) = out.track_in.upgrade()
                && let Some(rid) = chosen_rid(&track.rids, layer)
            {
                keyframes.push((track.mid, rid));
            }
        }
        keyframes
    }

    /// The `(origin, layer)` this receiver currently gets forwarded from
    /// every other peer's camera it subscribes to (one entry per origin;
    /// screens and audio are never layer-filtered). A keyframe-gated
    /// switch shows up once it has cut over. Exposed via `/metrics` so
    /// layer switches are observable without a browser.
    pub(crate) fn layer_preferences(&self) -> Vec<(PeerId, VideoLayer)> {
        let mut prefs = Vec::new();
        for out in &self.tracks_out {
            if out.screen || !matches!(out.kind, MediaKind::Video) {
                continue;
            }
            if !prefs.iter().any(|(origin, _)| *origin == out.origin) {
                prefs.push((out.origin, out.layer));
            }
        }
        prefs
    }

    /// The rid this receiver currently wants for `origin`'s track
    /// `mid_in`: `None` when the track is not simulcast (the caller then
    /// asks the origin for a whole-track keyframe).
    pub(crate) fn chosen_rid_for(&self, origin: PeerId, mid_in: Mid) -> Option<Rid> {
        let out = self
            .tracks_out
            .iter()
            .find(|t| t.origin == origin && t.mid_in == mid_in)?;
        let track = out.track_in.upgrade()?;
        chosen_rid(&track.rids, out.layer)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn forward_backpressure_sheds_while_other_errors_disconnect() {
        let addr: SocketAddr = "10.0.0.1:40000".parse().unwrap();
        let mut peer = Peer::new(PeerId::new_v4(), addr, &SimulcastConfig::default()).unwrap();

        // Egress backpressure: str0m already dropped the frame, the
        // peer must survive and the shed must be counted.
        peer.handle_forward_error(SfuError::Rtc(RtcError::WriteWithoutPoll));
        assert!(peer.is_alive(), "backpressure must not be fatal");
        assert_eq!(peer.dropped_frames, 1);

        // Any other write error is still fatal for the peer.
        peer.handle_forward_error(SfuError::Rtc(RtcError::NoSenderSource));
        assert!(!peer.is_alive(), "genuine write errors must disconnect");
    }
}
