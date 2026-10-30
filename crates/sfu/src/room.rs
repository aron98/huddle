use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use huddle_protocol::VideoLayer;
use str0m::Input;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaData, MediaKind, Mid};

use crate::alloc::{AUDIO_RESERVE, HIGH_COST, SCREEN_RESERVE, Source};
use crate::error::SfuError;
use crate::peer::{Peer, PollOutcome, TrackIn};
use crate::recorder::{RecordedFrame, RecorderSink};
use crate::{Bitrate, PeerId, PeerStats, SimulcastConfig};

use std::sync::Arc;

/// Cadence of the per-receiver layer allocation rounds.
const REALLOCATE_INTERVAL: Duration = Duration::from_millis(100);

/// An output event from polling the room, with all data owned.
#[derive(Debug)]
pub enum SfuOutput {
    /// Send these bytes to `destination` on the media socket.
    Transmit {
        peer: PeerId,
        destination: SocketAddr,
        contents: Vec<u8>,
    },
    /// Poll the room again no later than this instant.
    Timeout(Instant),
    /// The peer finished ICE + DTLS and is now connected.
    PeerConnected(PeerId),
    /// The peer died (poll or input error, or ICE disconnect) and has
    /// been removed from the room.
    PeerGone(PeerId),
    /// A renegotiation offer for `peer` to be relayed over signaling;
    /// the answer must come back via [`Room::accept_sdp_answer`].
    SdpOffer { peer: PeerId, sdp: String },
}

/// A multipoint room: owns every participant's [`Peer`] and routes media
/// between them. Sans-io — the embedder drives sockets and time.
pub struct Room {
    media_addr: SocketAddr,
    peers: HashMap<PeerId, Peer>,
    /// The recording tap, if the meeting is being recorded. Held as an
    /// `Arc` so dropping it (stop recording / empty meeting) is the
    /// recorder's shutdown signal; `None` taps nothing.
    recorder: Option<Arc<dyn RecorderSink>>,
    simulcast: SimulcastConfig,
    /// When the next layer-allocation round is due.
    next_reallocate: Option<Instant>,
}

impl Room {
    /// Create an empty room whose peers advertise `media_addr` as their
    /// ICE candidate, with the default simulcast settings. Also installs
    /// str0m's process-default crypto provider (idempotent).
    pub fn new(media_addr: SocketAddr) -> Room {
        Room::with_simulcast(media_addr, SimulcastConfig::default())
    }

    /// Like [`Room::new`], with explicit simulcast settings for every
    /// peer the room creates.
    pub fn with_simulcast(media_addr: SocketAddr, simulcast: SimulcastConfig) -> Room {
        str0m::crypto::from_feature_flags().install_process_default();
        Room {
            media_addr,
            peers: HashMap::new(),
            recorder: None,
            simulcast,
            next_reallocate: None,
        }
    }

    /// Install (or remove) the recording tap. Frames that pass the
    /// forwarding gate are handed to `recorder` until it is swapped out
    /// or dropped — the sink treats the drop as end-of-recording.
    pub fn set_recorder(&mut self, recorder: Option<Arc<dyn RecorderSink>>) {
        self.recorder = recorder;
    }

    /// The media socket address peers are told to send to.
    pub fn media_addr(&self) -> SocketAddr {
        self.media_addr
    }

    /// Register a new participant. Existing participants' incoming tracks
    /// are replayed to the newcomer as outgoing tracks, so media that
    /// started before the join still reaches them.
    pub fn create_peer(&mut self, id: PeerId) -> Result<(), SfuError> {
        if self.peers.contains_key(&id) {
            return Err(SfuError::PeerExists);
        }
        let mut peer = Peer::new(id, self.media_addr, &self.simulcast)?;
        for existing in self.peers.values() {
            for track in existing.tracks_in() {
                peer.open_track(track);
            }
        }
        self.peers.insert(id, peer);
        Ok(())
    }

    /// Remove a participant, disconnecting its `Rtc`. Every remaining
    /// peer's forwarding table is swept so tracks sourced by the departed
    /// peer are wound down (and their m-lines stopped) in renegotiation.
    pub fn remove_peer(&mut self, id: PeerId) {
        if let Some(mut peer) = self.peers.remove(&id) {
            peer.disconnect();
        }
        for peer in self.peers.values_mut() {
            peer.reap_dead_tracks();
        }
    }

    /// All peer ids, snapshot for iteration.
    pub fn peer_ids(&self) -> Vec<PeerId> {
        self.peers.keys().copied().collect()
    }

    /// Number of peers currently in the room.
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Per-peer media counters and link stats (testable, exposed via `/metrics`).
    pub fn peer_stats(&self, id: PeerId) -> Option<PeerStats> {
        self.peers.get(&id).map(|p| {
            let l = p.link_stats().unwrap_or_default();
            PeerStats {
                rx_frames: p.rx_frames,
                tx_frames: p.tx_frames,
                dropped_frames: p.dropped_frames,
                keyframes_requested: p.keyframes_requested,
                bytes_rx: l.bytes_rx,
                bytes_tx: l.bytes_tx,
                bitrate_in_bps: l.bitrate_in_bps,
                bitrate_out_bps: l.bitrate_out_bps,
                rtt_ms: l.rtt_ms,
                ingress_loss_fraction: l.ingress_loss_fraction,
                egress_loss_fraction: l.egress_loss_fraction,
                layer_preferences: p.layer_preferences(),
            }
        })
    }

    /// `(incoming, outgoing)` track counts for one peer — outgoing counts
    /// every entry, including ones being torn down. For tests and the
    /// future `/stats` endpoint.
    pub fn peer_tracks(&self, id: PeerId) -> Option<(usize, usize)> {
        self.peers
            .get(&id)
            .map(|p| (p.tracks_in_count(), p.tracks_out_count()))
    }

    /// Answer a peer's SDP offer.
    ///
    /// If the offer flipped a screen share's state (stop or resume on a
    /// reused transceiver), resumes are fanned back out immediately;
    /// stops converge through each subscriber's negotiation sweep.
    pub fn answer_sdp_offer(&mut self, id: PeerId, sdp: &str) -> Result<String, SfuError> {
        let (answer, screens) = self.peer_mut(id)?.answer_sdp_offer(sdp)?;
        for flip in screens {
            if flip.active {
                self.fan_out_track(&flip.track);
            }
        }
        Ok(answer)
    }

    /// Record a participant's screen-share announcement.
    ///
    /// `true` arms the peer so its next remote video m-line is treated
    /// as the screen track; `false` stands down any live share — the
    /// sharer may stop without touching the SDP (e.g. the browser UI
    /// stop button races the removeTrack offer), so the SFU deactivates
    /// the track here too, idempotently with the direction-change path.
    pub fn set_sharing(&mut self, id: PeerId, sharing: bool) -> Result<(), SfuError> {
        let origin = self.peer_mut(id)?;
        origin.set_sharing(sharing);
        if sharing {
            return Ok(());
        }
        // Wake every subscriber's negotiation so the stops are offered
        // promptly rather than on their next unrelated renegotiation.
        for peer in self.peers.values_mut() {
            peer.sweep_screens();
        }
        Ok(())
    }

    /// Deliver a peer's answer to a renegotiation offer we sent.
    ///
    /// Every track that just went live gets a keyframe request fan-out to
    /// its source: a fresh subscriber joins mid-GOP, and without this its
    /// first tiles would stay black until the next PLI. Simulcast sources
    /// are asked on the rid matching the subscriber's current layer.
    pub fn accept_sdp_answer(&mut self, id: PeerId, sdp: &str) -> Result<(), SfuError> {
        let opened = self.peer_mut(id)?.accept_sdp_answer(sdp)?;
        for (origin, mid_in) in opened {
            let rid = self
                .peers
                .get(&id)
                .and_then(|p| p.chosen_rid_for(origin, mid_in));
            if let Some(origin_peer) = self.peers.get_mut(&origin)
                && let Err(e) = origin_peer.request_keyframe(mid_in, rid, KeyframeRequestKind::Fir)
            {
                tracing::debug!("subscription keyframe request to {origin} failed: {e}");
            }
        }
        Ok(())
    }

    /// Deliver a trickled ICE candidate from a peer.
    pub fn add_remote_candidate(&mut self, id: PeerId, candidate: &str) -> Result<(), SfuError> {
        self.peer_mut(id)?.add_remote_candidate(candidate)
    }

    /// The legacy per-origin layer preference (`LayerPreference`),
    /// mapped onto the focus hint (ADR 015, D5): `High` focuses `origin`,
    /// `Low` clears the focus if `origin` holds it and is otherwise a
    /// no-op. The SFU arbitrates the actual layers — a `Low` is never a
    /// cap, so old clients that send it for every unfocused tile still
    /// get the default High while bandwidth allows.
    pub fn set_layer_preference(
        &mut self,
        receiver: PeerId,
        origin: PeerId,
        layer: VideoLayer,
    ) -> Result<(), SfuError> {
        let focus = self.peer_mut(receiver)?.focus();
        match layer {
            VideoLayer::High => self.set_focus(receiver, Some(origin)),
            VideoLayer::Low if focus == Some(origin) => self.set_focus(receiver, None),
            VideoLayer::Low => Ok(()),
        }
    }

    /// Set `receiver`'s focused (staged) participant. Focus wins the
    /// High layer when not every camera can have it; it never forces a
    /// layer the receiver's bandwidth cannot carry. Takes effect at once.
    pub fn set_focus(&mut self, receiver: PeerId, focus: Option<PeerId>) -> Result<(), SfuError> {
        self.peer_mut(receiver)?.set_focus(focus);
        self.reallocate(receiver, Instant::now());
        Ok(())
    }

    /// Pin `peer`'s egress estimate for layer allocation (`None`
    /// returns it to str0m's measurement). The sans-io seam that makes
    /// layer decisions deterministic in tests; see also
    /// [`SimulcastConfig::estimate_override`].
    pub fn set_egress_estimate_override(
        &mut self,
        peer: PeerId,
        estimate: Option<Bitrate>,
    ) -> Result<(), SfuError> {
        self.peer_mut(peer)?.set_estimate_override(estimate);
        Ok(())
    }

    /// Apply a (partial) mute update for a peer. Frames of a muted kind
    /// arriving from that peer are no longer forwarded to the room.
    pub fn set_mute(
        &mut self,
        id: PeerId,
        audio_muted: Option<bool>,
        video_muted: Option<bool>,
    ) -> Result<(), SfuError> {
        self.peer_mut(id)?.set_mute(audio_muted, video_muted);
        Ok(())
    }

    /// The peer (if any) that accepts this input. This is the demultiplex
    /// step for datagrams arriving on the shared media socket.
    pub fn accepts(&self, input: &Input) -> Option<PeerId> {
        match input {
            Input::Receive(..) => self
                .peers
                .iter()
                .find(|(_, p)| p.accepts(input))
                .map(|(&id, _)| id),
            // A bare timeout is not tied to a single peer; feed it to all
            // via `handle_timeout` instead.
            Input::Timeout(_) => None,
        }
    }

    /// Feed a network input into the room. `Receive` inputs are routed to
    /// the (single) accepting peer — errors disconnect it — and `Timeout`
    /// inputs advance every peer's state machines.
    ///
    /// Returns the id of the peer that accepted a `Receive` input.
    pub fn handle_input(&mut self, input: Input) -> Option<PeerId> {
        match input {
            Input::Receive(..) => {
                let id = self.accepts(&input)?;
                if let Some(peer) = self.peers.get_mut(&id)
                    && let Err(e) = peer.handle_input(input)
                {
                    tracing::warn!("peer {id} input error, disconnecting: {e}");
                    peer.disconnect();
                }
                Some(id)
            }
            Input::Timeout(now) => {
                self.handle_timeout(now);
                None
            }
        }
    }

    /// Advance every peer's internal state machines to `now`, and run
    /// the simulcast layer allocation when its round is due.
    pub fn handle_timeout(&mut self, now: Instant) {
        for peer in self.peers.values_mut() {
            peer.handle_timeout(now);
        }
        if self.next_reallocate.is_none_or(|t| now >= t) {
            self.next_reallocate = Some(now + REALLOCATE_INTERVAL);
            for id in self.peer_ids() {
                self.reallocate(id, now);
            }
        }
    }

    /// One layer-allocation round for `receiver` (ADR 015): gather its
    /// simulcast cameras (with per-rid liveness from their origins) and
    /// its non-adaptive reserve, let its allocator decide, and ask the
    /// origins of switched layers for keyframes.
    fn reallocate(&mut self, receiver: PeerId, now: Instant) {
        let Some(peer) = self.peers.get(&receiver) else {
            return;
        };
        let mut sources: Vec<Source> = Vec::new();
        let mut reserve = Bitrate::ZERO;
        for (seq, track) in peer.subscriptions() {
            let origin = self.peers.get(&track.origin);
            if track.kind == MediaKind::Audio {
                reserve = reserve + AUDIO_RESERVE;
            } else if track.screen {
                if track.active() {
                    reserve = reserve
                        + origin
                            .and_then(|o| o.ingress_bps(track.mid))
                            .map_or(SCREEN_RESERVE, Bitrate::bps);
                }
            } else if track.rids.is_empty() {
                // A single-layer camera is forwarded as-is.
                reserve = reserve + HIGH_COST;
            } else if !sources.iter().any(|s| s.origin == track.origin) {
                let live = |rid| origin.is_some_and(|o| o.rid_live(track.mid, rid, now));
                sources.push(Source {
                    origin: track.origin,
                    order: seq,
                    high_live: live(track.rids.first().copied()),
                    low_live: live(track.rids.last().copied()),
                });
            }
        }
        let Some(peer) = self.peers.get_mut(&receiver) else {
            return;
        };
        for (origin, mid_in, rid) in peer.reallocate(now, &sources, reserve) {
            if let Some(origin_peer) = self.peers.get_mut(&origin) {
                origin_peer.request_keyframe_throttled(mid_in, Some(rid), KeyframeRequestKind::Fir);
            }
        }
    }

    /// Poll one peer until it yields an output for the embedder.
    ///
    /// Internal outcomes (fan-out, media routing, keyframe relay) are
    /// processed inline and polling continues, so a single call can make
    /// substantive progress. Returns `None` once the peer is unknown or
    /// has nothing more to produce.
    pub fn poll_output(&mut self, id: PeerId) -> Option<SfuOutput> {
        loop {
            let peer = self.peers.get_mut(&id)?;
            if !peer.is_alive() {
                break;
            }
            if let Some(sdp) = peer.negotiate_if_needed() {
                return Some(SfuOutput::SdpOffer { peer: id, sdp });
            }

            match peer.poll_once() {
                Ok(PollOutcome::Transmit {
                    destination,
                    contents,
                }) => {
                    return Some(SfuOutput::Transmit {
                        peer: id,
                        destination,
                        contents,
                    });
                }
                Ok(PollOutcome::Timeout(t)) => return Some(SfuOutput::Timeout(t)),
                Ok(PollOutcome::Connected) => return Some(SfuOutput::PeerConnected(id)),
                Ok(PollOutcome::TrackOpened(track)) => {
                    self.fan_out_track(&track);
                    continue;
                }
                Ok(PollOutcome::Media(data)) => {
                    self.route_media(id, &data);
                    continue;
                }
                Ok(PollOutcome::KeyframeRequested(req)) => {
                    self.route_keyframe_request(id, req);
                    continue;
                }
                Ok(PollOutcome::Ignored) => continue,
                Ok(PollOutcome::Gone) => break,
                Err(e) => {
                    tracing::warn!("peer {id} poll error, removing: {e}");
                    break;
                }
            };
        }

        self.remove_peer(id);
        Some(SfuOutput::PeerGone(id))
    }

    fn peer_mut(&mut self, id: PeerId) -> Result<&mut Peer, SfuError> {
        self.peers.get_mut(&id).ok_or(SfuError::UnknownPeer)
    }

    /// Give every *other* peer an outgoing handle to `origin`'s track.
    ///
    /// Idempotent: existing entries for the same source track are
    /// resumed (a screen share that stopped and came back) rather than
    /// duplicated.
    fn fan_out_track(&mut self, track: &Arc<TrackIn>) {
        for (pid, peer) in self.peers.iter_mut() {
            if *pid != track.origin {
                peer.open_or_resume_track(track);
            }
        }
    }

    /// Deliver `origin`'s media frame to every other peer that has the
    /// corresponding outgoing track open.
    fn route_media(&mut self, origin: PeerId, data: &MediaData) {
        if let Some(peer) = self.peers.get_mut(&origin) {
            peer.rx_frames += 1;
            peer.note_frame(data.mid, data.rid, data.network_time);
            if !data.contiguous {
                peer.request_keyframe_all_layers_throttled(data.mid, KeyframeRequestKind::Fir);
            }
            // Muted at the source: the frame is counted but not forwarded.
            if !peer.is_forwardable(data.mid) {
                return;
            }
        }

        self.record_frame(origin, data);

        let targets: Vec<(PeerId, Mid)> = self
            .peers
            .iter()
            .filter(|entry| *entry.0 != origin)
            .filter_map(|entry| {
                entry
                    .1
                    .outgoing_mid_for(origin, data.mid)
                    .map(|mid_out| (*entry.0, mid_out))
            })
            .collect();

        for (pid, mid_out) in targets {
            let Some(target) = self.peers.get_mut(&pid) else {
                continue;
            };
            if let Err(e) = target.forward_media(mid_out, data) {
                target.handle_forward_error(e);
            }
        }
    }

    /// Relay a receiver's keyframe request (on its outgoing `mid`) back to
    /// the origin peer's incoming track.
    fn route_keyframe_request(&mut self, receiver: PeerId, req: KeyframeRequest) {
        let Some(peer) = self.peers.get(&receiver) else {
            return;
        };
        let Some((origin, mid_in)) = peer.track_in_for_outgoing(req.mid) else {
            return;
        };
        if let Some(origin_peer) = self.peers.get_mut(&origin)
            && let Err(e) = origin_peer.request_keyframe(mid_in, req.rid, req.kind)
        {
            tracing::warn!("keyframe relay to peer {origin} failed: {e}");
        }
    }

    /// Tap `origin`'s frame into the recording sink, if one is installed.
    ///
    /// Called after the forwarding gate, so a recording contains exactly
    /// what other participants receive (muted sources record nothing).
    /// Fail-open: an absent sink — or a track we cannot resolve — records
    /// nothing and never disturbs the forwarding path.
    fn record_frame(&self, origin: PeerId, data: &MediaData) {
        let Some(recorder) = &self.recorder else {
            return;
        };
        let Some(track) = self
            .peers
            .get(&origin)
            .and_then(|peer| peer.track_in(data.mid))
        else {
            return;
        };
        recorder.record(RecordedFrame {
            origin,
            kind: track.kind,
            screen: track.screen,
            pt: *data.pt,
            seq: data.seq_range.start().as_u16(),
            timestamp: data.time.numer(),
            contiguous: data.contiguous,
            keyframe: data.is_keyframe(),
            payload: data.data.clone(),
        });
    }
}
