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
    ChatMessage, ClientMessage, ErrorCode, MeetingDto, PROTOCOL_VERSION, Participant, ServerMessage,
};

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

    /// Mutable access to the peer connection, e.g. to attach local media
    /// before the first offer.
    pub fn peer_mut(&mut self) -> &mut P {
        &mut self.peer
    }

    /// Send `Join` and wait for `Welcome`.
    pub async fn join(&mut self, meeting_code: &str, name: &str) -> Result<(), RtcError> {
        if self.state != SessionState::New {
            return Err(RtcError::NotJoined);
        }
        self.state = SessionState::Joining;
        self.signaling
            .send(ClientMessage::Join {
                meeting_code: meeting_code.to_owned(),
                name: name.to_owned(),
                token: None,
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
                    ..
                } => {
                    if protocol_version != PROTOCOL_VERSION {
                        self.state = SessionState::Closed;
                        return Err(RtcError::ProtocolVersion(protocol_version));
                    }
                    self.you = Some(you);
                    self.meeting = Some(meeting);
                    self.roster = roster;
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
            let answer = self.peer.apply_remote_offer(sdp).await?;
            self.signaling
                .send(ClientMessage::SdpAnswer { sdp: answer })
                .await?;
            self.flush_local_candidates().await?;
        }
        Ok(())
    }

    /// Read and process the next server message. Returns the raw message so
    /// the UI layer can react (draw tiles, update chat, show errors).
    pub async fn next_server_message(&mut self) -> Result<ServerMessage, RtcError> {
        let Some(message) = self.signaling.next_message().await else {
            self.state = SessionState::Closed;
            return Err(RtcError::Closed);
        };
        let message = message?;
        self.handle_server_message(message.clone()).await?;
        Ok(message)
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
            // Surfaced to the UI in a follow-up commit.
            ServerMessage::Speaking { .. } => Ok(()),
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
