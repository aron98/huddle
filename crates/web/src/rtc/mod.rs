//! `RtcClient`: the signaling driver shared by the browser and tests.
//!
//! Owns the sequencing rules of the huddle wire protocol — join handshake,
//! SDP negotiation in both directions, ICE trickle, roster bookkeeping, chat,
//! and mute state — while staying agnostic of *how* messages travel
//! ([`SignalingStream`]) and *how* WebRTC happens ([`PeerConnection`]). The
//! browser wires these traits to `WebSocket` and `RTCPeerConnection`
//! respectively; native tests wire them to a mock server and a fake.

mod peer;
mod transport;

#[cfg(target_arch = "wasm32")]
pub mod browser;

#[cfg(target_arch = "wasm32")]
pub use browser::{BrowserPeerConnection, BrowserSignalingStream};
pub use peer::{LocalCandidate, PeerConnection, PeerError};
pub use transport::{SignalingError, SignalingStream};

use huddle_protocol::{
    ChatMessage, ClientMessage, ErrorCode, MeetingDto, PROTOCOL_VERSION, Participant,
    ParticipantId, ServerMessage, VideoLayer,
};
use std::collections::HashSet;

/// Build the signaling WebSocket URL from the page's location. Browsers
/// refuse `ws://` connections from `https://` pages (mixed content), so the
/// scheme must mirror the page protocol: `wss` on HTTPS origins, `ws`
/// otherwise (local dev and the HTTP e2e farm).
pub fn signaling_url(protocol: &str, host: &str) -> String {
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    format!("{scheme}://{host}/ws")
}

#[cfg(test)]
mod tests {
    use super::signaling_url;

    #[test]
    fn https_pages_get_wss() {
        assert_eq!(
            signaling_url("https:", "meet.example.com"),
            "wss://meet.example.com/ws"
        );
        assert_eq!(
            signaling_url("https:", "example.com:8443"),
            "wss://example.com:8443/ws"
        );
    }

    #[test]
    fn http_pages_get_ws() {
        assert_eq!(
            signaling_url("http:", "localhost:3000"),
            "ws://localhost:3000/ws"
        );
        assert_eq!(
            signaling_url("http:", "127.0.0.1:3131"),
            "ws://127.0.0.1:3131/ws"
        );
    }

    #[test]
    fn non_http_schemes_fall_back_to_ws() {
        // Unknown/exotic origins (e.g. file:) stay on the plain WebSocket
        // scheme, matching the pre-fix behaviour for local dev.
        assert_eq!(signaling_url("file:", "localhost"), "ws://localhost/ws");
        assert_eq!(signaling_url("", "localhost"), "ws://localhost/ws");
    }
}

/// Everything that can go wrong above the two transport boundaries.
#[derive(Debug, thiserror::Error)]
pub enum RtcError {
    #[error("signaling: {0}")]
    Signaling(#[from] SignalingError),
    #[error("peer connection: {0}")]
    Peer(#[from] PeerError),
    #[error("server refused: {0}")]
    Server(#[from] ServerError),
    #[error("server speaks protocol version {0}, we speak {PROTOCOL_VERSION}")]
    ProtocolVersion(u32),
    #[error("not in a meeting")]
    NotJoined,
    #[error("connection closed")]
    Closed,
}

/// A server-sent [`ServerMessage::Error`].
#[derive(Debug, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ServerError {
    pub code: ErrorCode,
    pub message: String,
}

/// The client's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Not joined; only [`RtcClient::join`] is valid.
    New,
    /// Join sent, awaiting `Welcome`.
    Joining,
    /// Inside the meeting.
    InCall,
    /// Left (or kicked); the client is done.
    Closed,
}

/// Drives one meeting session over a signaling stream and a peer connection.
pub struct RtcClient<S, P> {
    signaling: S,
    peer: P,
    state: SessionState,
    you: Option<Participant>,
    roster: Vec<Participant>,
    meeting: Option<MeetingDto>,
    chat_log: Vec<ChatMessage>,
    /// Participants the server last reported as speaking.
    speaking: HashSet<ParticipantId>,
    /// Whether the meeting is currently being recorded (consent indicator).
    recording: bool,
    /// Whether the meeting is end-to-end encrypted (issue #27); the
    /// E2EE plumbing in `crate::e2ee` only runs when this is set.
    e2ee_enabled: bool,
    /// The meeting's current E2EE epoch, bumped by the server on every
    /// join/leave (authoritative on all sides; see ADR 010).
    e2ee_epoch: u64,
    /// E2EE messages (epoch rotations + keying envelopes) that the
    /// inner pump loops consumed without a raw-message path back to
    /// the UI; `crate::e2ee::session` drains them (issue #27).
    pending_e2ee: Vec<ServerMessage>,
}

impl<S: SignalingStream, P: PeerConnection> RtcClient<S, P> {
    pub fn new(signaling: S, peer: P) -> Self {
        Self {
            signaling,
            peer,
            state: SessionState::New,
            you: None,
            roster: Vec::new(),
            meeting: None,
            chat_log: Vec::new(),
            speaking: HashSet::new(),
            recording: false,
            e2ee_enabled: false,
            e2ee_epoch: 0,
            pending_e2ee: Vec::new(),
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Our own participant, set once the server says welcome.
    pub fn you(&self) -> Option<&Participant> {
        self.you.as_ref()
    }

    pub fn meeting(&self) -> Option<&MeetingDto> {
        self.meeting.as_ref()
    }

    pub fn roster(&self) -> &[Participant] {
        &self.roster
    }

    pub fn chat_log(&self) -> &[ChatMessage] {
        &self.chat_log
    }

    /// Mutable access to the buffered chat log: the E2EE layer rewrites
    /// bodies it could not decrypt when they arrived (replayed history and
    /// anything swallowed while `publish` was mid-negotiation) once the
    /// keys for their epoch show up.
    pub fn chat_log_mut(&mut self) -> &mut [ChatMessage] {
        &mut self.chat_log
    }

    /// Participants the server currently reports as speaking.
    pub fn speaking(&self) -> &HashSet<ParticipantId> {
        &self.speaking
    }

    /// Whether the meeting is currently being recorded.
    pub fn recording(&self) -> bool {
        self.recording
    }

    /// Whether the meeting runs in end-to-end encrypted mode.
    pub fn e2ee_enabled(&self) -> bool {
        self.e2ee_enabled
    }

    /// The meeting's current E2EE epoch (0 when the meeting is plaintext).
    pub fn e2ee_epoch(&self) -> u64 {
        self.e2ee_epoch
    }

    /// Take the E2EE messages buffered by `handle_server_message` —
    /// epoch rotations and relayed keying envelopes destined for the
    /// wasm-only keying state machine (`crate::e2ee::session`).
    pub fn take_pending_e2ee(&mut self) -> Vec<ServerMessage> {
        std::mem::take(&mut self.pending_e2ee)
    }

    /// Mutable access to the peer connection, e.g. to attach local media
    /// before the first offer.
    pub fn peer_mut(&mut self) -> &mut P {
        &mut self.peer
    }

    /// Send `Join` and wait for `Welcome`. `token` carries the per-meeting
    /// passcode, if the meeting has one.
    pub async fn join(
        &mut self,
        meeting_code: &str,
        name: &str,
        token: Option<String>,
    ) -> Result<(), RtcError> {
        if self.state != SessionState::New {
            return Err(RtcError::NotJoined);
        }
        self.state = SessionState::Joining;
        self.signaling
            .send(ClientMessage::Join {
                meeting_code: meeting_code.to_owned(),
                name: name.to_owned(),
                token,
            })
            .await?;

        loop {
            let Some(message) = self.signaling.next_message().await else {
                self.state = SessionState::Closed;
                return Err(RtcError::Closed);
            };
            match message? {
                ServerMessage::Welcome {
                    protocol_version,
                    you,
                    meeting,
                    roster,
                    ice_servers,
                    recording,
                    e2ee_enabled,
                    e2ee_epoch,
                } => {
                    if protocol_version != PROTOCOL_VERSION {
                        self.state = SessionState::Closed;
                        return Err(RtcError::ProtocolVersion(protocol_version));
                    }
                    // The peer connection exists before the join, so the
                    // TURN/STUN servers the server advertises arrive only
                    // now — apply them before any offer is created
                    // (`publish` runs after this returns).
                    self.peer.update_ice_servers(&ice_servers)?;
                    self.you = Some(you);
                    self.meeting = Some(meeting);
                    self.roster = roster;
                    self.recording = recording;
                    self.e2ee_enabled = e2ee_enabled;
                    self.e2ee_epoch = e2ee_epoch;
                    self.state = SessionState::InCall;
                    return Ok(());
                }
                ServerMessage::Error { code, message } => {
                    self.state = SessionState::Closed;
                    return Err(RtcError::Server(ServerError { code, message }));
                }
                // Anything else before the welcome is a protocol violation;
                // keep waiting rather than fail closed on future additions.
                _ => continue,
            }
        }
    }

    /// Offer our local media to the SFU and wait for its answer.
    ///
    /// The SFU may send its own renegotiation offer (another participant's
    /// tracks) while our offer is in flight. Applying it would move the
    /// peer connection out of `have-local-offer` and make the SFU's answer
    /// inapplicable ("wrong state"), so server offers are queued and
    /// applied only once our own negotiation finishes.
    pub async fn publish(&mut self) -> Result<(), RtcError> {
        self.require_in_call()?;

        let offer = self.peer.create_offer().await?;
        self.signaling
            .send(ClientMessage::SdpOffer { sdp: offer })
            .await?;
        self.flush_local_candidates().await?;

        let mut deferred_offers = Vec::new();
        loop {
            let Some(message) = self.signaling.next_message().await else {
                self.state = SessionState::Closed;
                return Err(RtcError::Closed);
            };
            match message? {
                ServerMessage::SdpAnswer { sdp } => {
                    self.peer.apply_answer(sdp).await?;
                    break;
                }
                ServerMessage::Error { code, message } => {
                    return Err(RtcError::Server(ServerError { code, message }));
                }
                ServerMessage::SdpOffer { sdp } => deferred_offers.push(sdp),
                // Candidates and roster chatter can interleave; handle it.
                other => self.handle_server_message(other).await?,
            }
        }

        for sdp in deferred_offers {
            // A deferred offer can already be stale: the SFU discards its
            // own pending renegotiation whenever it accepts an offer from us
            // and re-offers from the rebased session, so one that hit the
            // wire before our own offer can no longer apply afterwards
            // ("m-line order doesn't match"). Failing here would kill a
            // healthy session — e.g. a share that preempts another one — so
            // drop the offer and let the regenerated one be answered.
            match self.peer.apply_remote_offer(sdp).await {
                Ok(answer) => {
                    self.signaling
                        .send(ClientMessage::SdpAnswer { sdp: answer })
                        .await?;
                    self.flush_local_candidates().await?;
                }
                Err(e) => leptos::logging::warn!("dropping stale server offer: {e}"),
            }
        }
        Ok(())
    }

    /// Read and process the next server message. Returns the raw message so
    /// the UI layer can react (draw tiles, update chat, show errors).
    ///
    /// NOT cancel-safe: `handle_server_message` awaits browser promises and
    /// sends signaling replies; dropping the returned future mid-flight can
    /// lose the reply. UI loops that race this against other futures should
    /// use [`RtcClient::read_server_message`] inside the `select!` and pass
    /// the result to [`RtcClient::handle_server_message`] afterwards.
    pub async fn next_server_message(&mut self) -> Result<ServerMessage, RtcError> {
        let message = self.read_server_message().await?;
        self.handle_server_message(message.clone()).await?;
        Ok(message)
    }

    /// Receive the next server message from the signaling stream without
    /// applying it. Cancel-safe: the only side effect on cancellation is the
    /// message staying buffered in the stream.
    pub async fn read_server_message(&mut self) -> Result<ServerMessage, RtcError> {
        let Some(message) = self.signaling.next_message().await else {
            self.state = SessionState::Closed;
            return Err(RtcError::Closed);
        };
        Ok(message?)
    }

    /// Apply one server message to our state, dispatching side effects to the
    /// peer connection and signaling stream as needed.
    pub async fn handle_server_message(&mut self, msg: ServerMessage) -> Result<(), RtcError> {
        match msg {
            ServerMessage::Welcome { .. } => Ok(()), // already joined; ignore
            ServerMessage::ParticipantJoined { participant } => {
                if let Some(existing) = self.roster.iter_mut().find(|p| p.id == participant.id) {
                    *existing = participant;
                } else {
                    self.roster.push(participant);
                }
                Ok(())
            }
            ServerMessage::ParticipantLeft { id } => {
                self.roster.retain(|p| p.id != id);
                self.speaking.remove(&id);
                Ok(())
            }
            ServerMessage::SdpAnswer { .. } => Ok(()), // consumed by `publish`
            ServerMessage::SdpOffer { sdp } => {
                let answer = self.peer.apply_remote_offer(sdp).await?;
                self.signaling
                    .send(ClientMessage::SdpAnswer { sdp: answer })
                    .await?;
                self.flush_local_candidates().await
            }
            ServerMessage::IceCandidate {
                candidate,
                sdp_mid,
                sdp_m_line_index,
            } => self
                .peer
                .add_remote_candidate(LocalCandidate {
                    candidate,
                    sdp_mid,
                    sdp_m_line_index,
                })
                .await
                .map_err(RtcError::from),
            ServerMessage::Chat { message } => {
                self.chat_log.push(message);
                Ok(())
            }
            ServerMessage::MuteState {
                id,
                audio_muted,
                video_muted,
            } => {
                if let Some(p) = self.roster.iter_mut().find(|p| p.id == id) {
                    p.audio_muted = audio_muted;
                    p.video_muted = video_muted;
                }
                Ok(())
            }
            ServerMessage::Speaking { id, speaking } => {
                if speaking {
                    self.speaking.insert(id);
                } else {
                    self.speaking.remove(&id);
                }
                Ok(())
            }
            ServerMessage::Sharing { id, sharing } => {
                if let Some(p) = self.roster.iter_mut().find(|p| p.id == id) {
                    p.sharing = sharing;
                }
                // `you` is a Welcome-time snapshot; keep it in sync so a
                // sharer can tell whether a start survived (a preemption
                // that lands mid-start flips this back to false before
                // `publish` returns).
                if let Some(you) = self.you.as_mut().filter(|you| you.id == id) {
                    you.sharing = sharing;
                }
                Ok(())
            }
            ServerMessage::Recording { active } => {
                self.recording = active;
                Ok(())
            }
            // Epoch bookkeeping is ours; the rotations and keying
            // envelopes are ALSO buffered for the E2EE module in
            // `crate::e2ee::session` (the browser-side crypto is
            // wasm-only, this driver is not) — buffering means they
            // survive inner pump loops (publish) that have no
            // raw-message path back to the UI.
            ServerMessage::E2eeEpoch { epoch } => {
                self.e2ee_epoch = epoch;
                self.pending_e2ee.push(ServerMessage::E2eeEpoch { epoch });
                Ok(())
            }
            ServerMessage::E2eeKeying {
                from,
                to,
                epoch,
                payload,
            } => {
                self.pending_e2ee.push(ServerMessage::E2eeKeying {
                    from,
                    to,
                    epoch,
                    payload,
                });
                Ok(())
            }
            ServerMessage::Error { code, message } => {
                Err(RtcError::Server(ServerError { code, message }))
            }
        }
    }

    /// Send a chat message. The server relays it back (via
    /// [`ServerMessage::Chat`]) to every participant including us.
    pub async fn send_chat(&mut self, body: String) -> Result<(), RtcError> {
        self.require_in_call()?;
        self.signaling
            .send(ClientMessage::Chat { body })
            .await
            .map_err(RtcError::from)
    }

    /// Send an opaque E2EE keying envelope to one participant (issue
    /// #27, ADR 010): the server cannot read the payload — only the
    /// addressee receives it.
    pub async fn send_e2ee_keying(
        &mut self,
        to: ParticipantId,
        epoch: u64,
        payload: String,
    ) -> Result<(), RtcError> {
        self.require_in_call()?;
        self.signaling
            .send(ClientMessage::E2eeKeying { to, epoch, payload })
            .await
            .map_err(RtcError::from)
    }

    /// Start sharing the staged screen track: announce the share first (the
    /// server labels the m-line our next offer creates as our screen), then
    /// attach the track and renegotiate via [`RtcClient::publish`].
    pub async fn start_screen_share(&mut self) -> Result<(), RtcError> {
        self.require_in_call()?;
        self.signaling
            .send(ClientMessage::Sharing { sharing: true })
            .await?;
        self.peer.add_screen_track().await?;
        self.publish().await
    }

    /// Stop sharing: withdraw the track, tell the server (it winds down the
    /// other subscribers), and renegotiate.
    pub async fn stop_screen_share(&mut self) -> Result<(), RtcError> {
        self.require_in_call()?;
        self.peer.remove_screen_track().await?;
        self.signaling
            .send(ClientMessage::Sharing { sharing: false })
            .await?;
        self.publish().await
    }

    /// Report our mute/camera state. The server broadcasts the canonical
    /// [`ServerMessage::MuteState`] back.
    pub async fn set_mute_state(
        &mut self,
        audio_muted: Option<bool>,
        video_muted: Option<bool>,
    ) -> Result<(), RtcError> {
        self.require_in_call()?;
        self.signaling
            .send(ClientMessage::MuteState {
                audio_muted,
                video_muted,
            })
            .await
            .map_err(RtcError::from)
    }

    /// Report whether we are speaking. The client is the authority on its
    /// own audio activity; the server echoes the canonical
    /// [`ServerMessage::Speaking`] back to everyone.
    pub async fn set_speaking(&mut self, speaking: bool) -> Result<(), RtcError> {
        self.require_in_call()?;
        self.signaling
            .send(ClientMessage::Speaking { speaking })
            .await
            .map_err(RtcError::from)
    }

    /// Tell the SFU which simulcast layer we want from `origin`'s camera
    /// (focused tile → high, unfocused → low). The SFU ignores it for
    /// audio and single-layer screen shares.
    pub async fn set_layer_preference(
        &mut self,
        origin: ParticipantId,
        layer: VideoLayer,
    ) -> Result<(), RtcError> {
        self.require_in_call()?;
        self.signaling
            .send(ClientMessage::LayerPreference { origin, layer })
            .await
            .map_err(RtcError::from)
    }

    /// Tell the SFU which participant we have on stage (`None` clears
    /// it). Only a hint: the SFU picks every camera's simulcast layer
    /// itself, and focus decides who wins the high layer when not every
    /// camera can have it.
    pub async fn set_focus(&mut self, origin: Option<ParticipantId>) -> Result<(), RtcError> {
        self.require_in_call()?;
        self.signaling
            .send(ClientMessage::Focus { origin })
            .await
            .map_err(RtcError::from)
    }

    /// Leave gracefully. Dropping the client also works (the socket close is
    /// the signal), but this way the roster updates are prompt.
    pub async fn leave(&mut self) -> Result<(), RtcError> {
        if self.state == SessionState::Closed {
            return Ok(());
        }
        let result = self.signaling.send(ClientMessage::Leave).await;
        self.state = SessionState::Closed;
        result.map_err(RtcError::from)
    }

    /// Drain candidates gathered locally and trickle them to the server.
    pub async fn flush_local_candidates(&mut self) -> Result<(), RtcError> {
        for candidate in self.peer.take_local_candidates() {
            self.signaling.send(candidate.into()).await?;
        }
        Ok(())
    }

    fn require_in_call(&self) -> Result<(), RtcError> {
        if self.state == SessionState::InCall {
            Ok(())
        } else {
            Err(RtcError::NotJoined)
        }
    }
}
