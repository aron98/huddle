use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use str0m::Input;
use str0m::media::{KeyframeRequest, KeyframeRequestKind, MediaData, Mid};

use crate::PeerId;
use crate::PeerStats;
use crate::error::SfuError;
use crate::peer::{Peer, PollOutcome, TrackIn};

use std::sync::Arc;

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
}

impl Room {
    /// Create an empty room whose peers advertise `media_addr` as their
    /// ICE candidate. Also installs str0m's process-default crypto
    /// provider (idempotent).
    pub fn new(media_addr: SocketAddr) -> Room {
        str0m::crypto::from_feature_flags().install_process_default();
        Room {
            media_addr,
            peers: HashMap::new(),
        }
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
        let mut peer = Peer::new(id, self.media_addr)?;
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

    /// Per-peer media counters (testable, later exposed via `/stats`).
    pub fn peer_stats(&self, id: PeerId) -> Option<PeerStats> {
        self.peers.get(&id).map(|p| PeerStats {
            rx_frames: p.rx_frames,
            tx_frames: p.tx_frames,
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
    pub fn answer_sdp_offer(&mut self, id: PeerId, sdp: &str) -> Result<String, SfuError> {
        self.peer_mut(id)?.answer_sdp_offer(sdp)
    }

    /// Deliver a peer's answer to a renegotiation offer we sent.
    pub fn accept_sdp_answer(&mut self, id: PeerId, sdp: &str) -> Result<(), SfuError> {
        self.peer_mut(id)?.accept_sdp_answer(sdp)
    }

    /// Deliver a trickled ICE candidate from a peer.
    pub fn add_remote_candidate(&mut self, id: PeerId, candidate: &str) -> Result<(), SfuError> {
        self.peer_mut(id)?.add_remote_candidate(candidate)
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

    /// Advance every peer's internal state machines to `now`.
    pub fn handle_timeout(&mut self, now: Instant) {
        for peer in self.peers.values_mut() {
            peer.handle_timeout(now);
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
                    self.fan_out_track(id, &track);
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

    /// Give every *other* peer an outgoing handle to `origin`'s new track.
    fn fan_out_track(&mut self, origin: PeerId, track: &Arc<TrackIn>) {
        for (pid, peer) in self.peers.iter_mut() {
            if *pid != origin {
                peer.open_track(track);
            }
        }
    }

    /// Deliver `origin`'s media frame to every other peer that has the
    /// corresponding outgoing track open.
    fn route_media(&mut self, origin: PeerId, data: &MediaData) {
        if let Some(peer) = self.peers.get_mut(&origin) {
            peer.rx_frames += 1;
            if !data.contiguous {
                peer.request_keyframe_throttled(data.mid, data.rid, KeyframeRequestKind::Fir);
            }
        }

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
                tracing::warn!("forward to peer {pid} failed, disconnecting: {e}");
                target.disconnect();
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
}
