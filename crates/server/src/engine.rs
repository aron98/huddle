//! The media engine: one thread that owns every live meeting room and the
//! media UDP socket, driven by commands from the async session tasks.
//!
//! str0m is sans-io: its `Rtc` instances want to be driven from a single
//! thread each, so the engine owns all [`Room`]s exclusively and runs a
//! classic poll loop — drain commands, poll outputs, sleep until the next
//! timer hint. Sessions (async axum tasks) talk to it through a plain
//! std channel; the engine talks back through per-session unbounded
//! channels. Ordering is therefore total: the engine processes commands
//! one at a time and appends to each session's outbound queue in order.
//!
//! A dedicated reader thread blocks on the media socket and forwards each
//! datagram into the command channel, which keeps the poll loop's sleep
//! `recv_timeout` responsive to packets.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use huddle_protocol::{ErrorCode, IceServer, PROTOCOL_VERSION, Participant, ServerMessage};
use huddle_sfu::{Room, SfuOutput};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::config::Config;
use crate::meetings::normalize_code;

/// Worst-case poll cadence when no peer asked to be woken earlier.
const MAX_SLEEP: Duration = Duration::from_millis(50);

/// Why a join was refused, mirrored from [`ErrorCode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinRejected {
    pub code: ErrorCode,
}

/// Everything an async session can ask the engine to do. Sessions are the
/// only writers; the reader thread also sends [`EngineCommand::Packet`].
pub enum EngineCommand {
    /// Put a participant in a meeting room. The session has already
    /// validated the meeting exists and its join window is open; the
    /// engine enforces capacity and name uniqueness.
    Join {
        id: Uuid,
        name: String,
        meeting: huddle_protocol::MeetingDto,
        out: UnboundedSender<ServerMessage>,
        reply: oneshot::Sender<Result<(), JoinRejected>>,
    },
    /// Answer a peer's SDP offer; the answer flows back via `out`.
    Sdp { code: String, id: Uuid, sdp: String },
    /// Deliver a peer's answer to a renegotiation offer we sent.
    SdpAnswer { code: String, id: Uuid, sdp: String },
    /// Deliver a trickled ICE candidate (candidate-string form).
    Ice {
        code: String,
        id: Uuid,
        candidate: String,
    },
    /// Relay a chat message to everyone in the meeting.
    Chat {
        code: String,
        id: Uuid,
        body: String,
    },
    /// Update a participant's mute state and broadcast it.
    Mute {
        code: String,
        id: Uuid,
        audio_muted: Option<bool>,
        video_muted: Option<bool>,
    },
    /// Remove a participant (graceful leave or socket gone).
    Leave { code: String, id: Uuid },
    /// A datagram arrived on the media socket.
    Packet { source: SocketAddr, bytes: Vec<u8> },
    /// Stop the engine thread (graceful shutdown of the process).
    Shutdown,
}

/// Cloneable handle sessions use to talk to the engine thread.
#[derive(Clone)]
pub struct EngineHandle {
    tx: Sender<EngineCommand>,
}

impl EngineHandle {
    pub fn join(
        &self,
        id: Uuid,
        name: String,
        meeting: huddle_protocol::MeetingDto,
        out: UnboundedSender<ServerMessage>,
    ) -> oneshot::Receiver<Result<(), JoinRejected>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(EngineCommand::Join {
            id,
            name,
            meeting,
            out,
            reply: reply_tx,
        });
        reply_rx
    }

    pub fn sdp(&self, code: String, id: Uuid, sdp: String) {
        let _ = self.tx.send(EngineCommand::Sdp { code, id, sdp });
    }

    pub fn sdp_answer(&self, code: String, id: Uuid, sdp: String) {
        let _ = self.tx.send(EngineCommand::SdpAnswer { code, id, sdp });
    }

    pub fn ice(&self, code: String, id: Uuid, candidate: String) {
        let _ = self.tx.send(EngineCommand::Ice {
            code,
            id,
            candidate,
        });
    }

    pub fn chat(&self, code: String, id: Uuid, body: String) {
        let _ = self.tx.send(EngineCommand::Chat { code, id, body });
    }

    pub fn mute(
        &self,
        code: String,
        id: Uuid,
        audio_muted: Option<bool>,
        video_muted: Option<bool>,
    ) {
        let _ = self.tx.send(EngineCommand::Mute {
            code,
            id,
            audio_muted,
            video_muted,
        });
    }

    pub fn leave(&self, code: String, id: Uuid) {
        let _ = self.tx.send(EngineCommand::Leave { code, id });
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(EngineCommand::Shutdown);
    }
}

/// One live meeting: the SFU room plus the bookkeeping the engine keeps
/// alongside it (roster, per-session outbound queues, taken names).
struct MeetingInstance {
    room: Room,
    participants: HashMap<Uuid, Participant>,
    outs: HashMap<Uuid, UnboundedSender<ServerMessage>>,
    names: HashSet<String>,
}

impl MeetingInstance {
    fn new(media_addr: SocketAddr) -> Self {
        Self {
            room: Room::new(media_addr),
            participants: HashMap::new(),
            outs: HashMap::new(),
            names: HashSet::new(),
        }
    }
}

/// The engine thread's world.
struct Engine {
    rx: Receiver<EngineCommand>,
    socket: UdpSocket,
    /// The address peers are told to send media to (advertised candidate).
    media_addr: SocketAddr,
    meetings: HashMap<String, MeetingInstance>,
    ice_servers: Vec<IceServer>,
    max_participants: usize,
}

/// Bind the media socket and start the engine + reader threads.
///
/// The socket binds to all interfaces on `config.media_port`; the port
/// actually bound (useful when 0 was requested) ends up in the returned
/// media address, which pairs `config.public_ip` with it.
pub fn spawn(config: &Config) -> anyhow::Result<(EngineHandle, SocketAddr)> {
    let socket = UdpSocket::bind(SocketAddr::new(
        IpAddr::from([0, 0, 0, 0]),
        config.media_port,
    ))?;
    let bound = socket.local_addr()?;
    let media_addr = SocketAddr::new(config.public_ip, bound.port());

    let (tx, rx) = std::sync::mpsc::channel::<EngineCommand>();

    // Reader thread: blocking recv, forward as commands. The read timeout
    // only exists so the thread notices engine shutdown promptly.
    let reader = socket.try_clone()?;
    let reader_tx = tx.clone();
    reader.set_read_timeout(Some(Duration::from_millis(500)))?;
    thread::Builder::new()
        .name("huddle-media-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                match reader.recv_from(&mut buf) {
                    Ok((n, source)) => {
                        if reader_tx
                            .send(EngineCommand::Packet {
                                source,
                                bytes: buf[..n].to_vec(),
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
            }
        })?;

    let ice_servers = config
        .ice_servers
        .iter()
        .map(|s| IceServer {
            urls: s.urls.clone(),
            username: s.username.clone(),
            credential: s.credential.clone(),
        })
        .collect();

    let engine = Engine {
        rx,
        socket,
        media_addr,
        meetings: HashMap::new(),
        ice_servers,
        max_participants: config.max_participants,
    };
    thread::Builder::new()
        .name("huddle-engine".into())
        .spawn(move || engine.run())?;

    Ok((EngineHandle { tx }, media_addr))
}

impl Engine {
    fn run(mut self) {
        loop {
            while let Ok(cmd) = self.rx.try_recv() {
                if matches!(cmd, EngineCommand::Shutdown) {
                    return;
                }
                self.handle(cmd);
            }

            let wake = self.poll_outputs();
            let sleep = wake
                .and_then(|t| t.checked_duration_since(Instant::now()))
                .unwrap_or(MAX_SLEEP)
                .min(MAX_SLEEP);

            match self.rx.recv_timeout(sleep) {
                Ok(cmd) => {
                    if matches!(cmd, EngineCommand::Shutdown) {
                        return;
                    }
                    self.handle(cmd);
                }
                Err(RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    for inst in self.meetings.values_mut() {
                        inst.room.handle_timeout(now);
                    }
                }
                // All handles dropped: nothing left to serve.
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Poll every peer of every room until each asks to sleep, handling
    /// outputs inline. Returns the earliest requested wake-up.
    fn poll_outputs(&mut self) -> Option<Instant> {
        let mut wake: Option<Instant> = None;
        let codes: Vec<String> = self.meetings.keys().cloned().collect();
        for code in &codes {
            let ids = self
                .meetings
                .get(code)
                .map(|i| i.room.peer_ids())
                .unwrap_or_default();
            for id in ids {
                while let Some(out) = self
                    .meetings
                    .get_mut(code)
                    .and_then(|inst| inst.room.poll_output(id))
                {
                    match out {
                        SfuOutput::Transmit {
                            destination,
                            contents,
                            ..
                        } => {
                            if let Err(e) = self.socket.send_to(&contents, destination) {
                                tracing::debug!("media send_to {destination} failed: {e}");
                            }
                        }
                        SfuOutput::Timeout(t) => {
                            wake = Some(wake.map_or(t, |w: Instant| w.min(t)));
                            break;
                        }
                        SfuOutput::PeerConnected(peer) => {
                            tracing::debug!(peer = %peer, "peer connected");
                        }
                        SfuOutput::SdpOffer { peer, sdp } => {
                            self.send_to(code, peer, ServerMessage::SdpOffer { sdp });
                        }
                        SfuOutput::PeerGone(peer) => {
                            self.remove_participant(code, peer);
                        }
                    }
                }
            }
        }
        wake
    }

    fn handle(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::Join {
                id,
                name,
                meeting,
                out,
                reply,
            } => self.join(id, name, meeting, out, reply),
            EngineCommand::Sdp { code, id, sdp } => self.answer_offer(&code, id, &sdp),
            EngineCommand::SdpAnswer { code, id, sdp } => {
                if let Some(inst) = self.meetings.get_mut(&code)
                    && let Err(e) = inst.room.accept_sdp_answer(id, &sdp)
                {
                    tracing::warn!(peer = %id, "bad sdp answer: {e}");
                }
            }
            EngineCommand::Ice {
                code,
                id,
                candidate,
            } => {
                // Browsers signal end-of-candidates with an empty string.
                if candidate.trim().is_empty() {
                    return;
                }
                if let Some(inst) = self.meetings.get_mut(&code)
                    && let Err(e) = inst.room.add_remote_candidate(id, &candidate)
                {
                    tracing::debug!(peer = %id, "bad ice candidate: {e}");
                }
            }
            EngineCommand::Chat { code, id, body } => self.relay_chat(&code, id, body),
            EngineCommand::Mute {
                code,
                id,
                audio_muted,
                video_muted,
            } => self.set_mute(&code, id, audio_muted, video_muted),
            EngineCommand::Leave { code, id } => self.remove_participant(&code, id),
            EngineCommand::Packet { source, bytes } => self.handle_packet(source, &bytes),
            EngineCommand::Shutdown => unreachable!("handled by the run loop"),
        }
    }

    fn join(
        &mut self,
        id: Uuid,
        name: String,
        meeting: huddle_protocol::MeetingDto,
        out: UnboundedSender<ServerMessage>,
        reply: oneshot::Sender<Result<(), JoinRejected>>,
    ) {
        let code = normalize_code(&meeting.code);
        let reject = |reply: oneshot::Sender<Result<(), JoinRejected>>, code: ErrorCode| {
            let _ = reply.send(Err(JoinRejected { code }));
        };

        if let Some(inst) = self.meetings.get(&code) {
            if inst.names.contains(&name.to_lowercase()) {
                reject(reply, ErrorCode::NameTaken);
                return;
            }
            if inst.participants.len() >= self.max_participants {
                reject(reply, ErrorCode::CapacityReached);
                return;
            }
        }

        let inst = self
            .meetings
            .entry(code.clone())
            .or_insert_with(|| MeetingInstance::new(self.media_addr));
        if let Err(e) = inst.room.create_peer(id) {
            tracing::error!(peer = %id, "create_peer failed: {e}");
            reject(reply, ErrorCode::Internal);
            return;
        }

        let participant = Participant {
            id,
            name: name.clone(),
            audio_muted: false,
            video_muted: false,
            sharing: false,
        };
        inst.names.insert(name.to_lowercase());
        inst.participants.insert(id, participant.clone());
        inst.outs.insert(id, out.clone());

        let roster: Vec<Participant> = inst.participants.values().cloned().collect();
        let _ = out.send(ServerMessage::Welcome {
            protocol_version: PROTOCOL_VERSION,
            you: participant.clone(),
            meeting,
            roster,
            ice_servers: self.ice_servers.clone(),
        });

        let joined = ServerMessage::ParticipantJoined { participant };
        for (pid, o) in &inst.outs {
            if *pid != id {
                let _ = o.send(joined.clone());
            }
        }

        let _ = reply.send(Ok(()));
    }

    fn answer_offer(&mut self, code: &str, id: Uuid, sdp: &str) {
        let Some(inst) = self.meetings.get_mut(code) else {
            return;
        };
        match inst.room.answer_sdp_offer(id, sdp) {
            Ok(answer) => {
                if let Some(out) = inst.outs.get(&id) {
                    let _ = out.send(ServerMessage::SdpAnswer { sdp: answer });
                }
            }
            Err(e) => {
                tracing::warn!(peer = %id, "bad sdp offer: {e}");
                let code = match e {
                    huddle_sfu::SfuError::UnknownPeer => return, // session already gone
                    huddle_sfu::SfuError::InvalidSdp(_) => ErrorCode::InvalidMessage,
                    _ => ErrorCode::Internal,
                };
                if let Some(out) = inst.outs.get(&id) {
                    let _ = out.send(ServerMessage::Error {
                        code,
                        message: "could not process the offer".into(),
                    });
                }
            }
        }
    }

    fn relay_chat(&mut self, code: &str, id: Uuid, body: String) {
        let Some(inst) = self.meetings.get_mut(code) else {
            return;
        };
        let Some(sender) = inst.participants.get(&id) else {
            return;
        };
        let message = huddle_protocol::ChatMessage {
            id: Uuid::new_v4(),
            sender_id: Some(id),
            sender_name: sender.name.clone(),
            body,
            sent_at: time::OffsetDateTime::now_utc(),
        };
        let msg = ServerMessage::Chat { message };
        for out in inst.outs.values() {
            let _ = out.send(msg.clone());
        }
    }

    fn set_mute(
        &mut self,
        code: &str,
        id: Uuid,
        audio_muted: Option<bool>,
        video_muted: Option<bool>,
    ) {
        let Some(inst) = self.meetings.get_mut(code) else {
            return;
        };
        let Some(p) = inst.participants.get_mut(&id) else {
            return;
        };
        if let Some(a) = audio_muted {
            p.audio_muted = a;
        }
        if let Some(v) = video_muted {
            p.video_muted = v;
        }
        let (audio_muted, video_muted) = (p.audio_muted, p.video_muted);
        let msg = ServerMessage::MuteState {
            id,
            audio_muted,
            video_muted,
        };
        for out in inst.outs.values() {
            let _ = out.send(msg.clone());
        }
    }

    /// Remove a participant: from the room, the roster, and the session
    /// map; tell everyone else; drop the whole instance when it empties.
    fn remove_participant(&mut self, code: &str, id: Uuid) {
        let Some(inst) = self.meetings.get_mut(code) else {
            return;
        };
        inst.room.remove_peer(id);
        if let Some(p) = inst.participants.remove(&id) {
            inst.names.remove(&p.name.to_lowercase());
            inst.outs.remove(&id);
            let msg = ServerMessage::ParticipantLeft { id };
            for out in inst.outs.values() {
                let _ = out.send(msg.clone());
            }
        }
        if inst.outs.is_empty() {
            self.meetings.remove(code);
        }
    }

    fn handle_packet(&mut self, source: SocketAddr, bytes: &[u8]) {
        let input = match str0m::net::Receive::new(
            str0m::net::Protocol::Udp,
            source,
            self.media_addr,
            bytes,
        ) {
            Ok(receive) => str0m::Input::Receive(Instant::now(), receive),
            Err(e) => {
                tracing::debug!("dropping malformed datagram from {source}: {e}");
                return;
            }
        };

        for inst in self.meetings.values_mut() {
            if inst.room.accepts(&input).is_some() {
                inst.room.handle_input(input);
                return;
            }
        }
    }

    fn send_to(&self, code: &str, id: Uuid, msg: ServerMessage) {
        if let Some(inst) = self.meetings.get(code)
            && let Some(out) = inst.outs.get(&id)
        {
            let _ = out.send(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use huddle_protocol::MeetingDto;
    use std::sync::Arc;
    use tokio::sync::mpsc::unbounded_channel;

    fn test_engine(max_participants: usize) -> (EngineHandle, Arc<()>) {
        // The Arc is a cheap way to keep the handle alive until the test
        // ends: dropping it disconnects the engine thread.
        let config = Config {
            media_port: 0,
            max_participants,
            ..Config::default()
        };
        let (handle, _media_addr) = spawn(&config).unwrap();
        (handle, Arc::new(()))
    }

    fn meeting_dto(code: &str) -> MeetingDto {
        MeetingDto {
            id: Uuid::new_v4(),
            code: code.into(),
            title: "Test".into(),
            starts_at: time::OffsetDateTime::now_utc(),
            ends_at: time::OffsetDateTime::now_utc() + time::Duration::hours(1),
            cancelled: false,
        }
    }

    /// A fake session: join and collect everything the engine sends back.
    async fn join(
        handle: &EngineHandle,
        code: &str,
        name: &str,
    ) -> (Uuid, tokio::sync::mpsc::UnboundedReceiver<ServerMessage>) {
        let id = Uuid::new_v4();
        let (out_tx, out_rx) = unbounded_channel();
        let reply = handle.join(id, name.to_owned(), meeting_dto(code), out_tx);
        reply.await.expect("engine alive").expect("join accepted");
        (id, out_rx)
    }

    async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerMessage>) -> ServerMessage {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("message within timeout")
            .expect("channel open")
    }

    #[tokio::test]
    async fn join_welcomes_and_broadcasts_roster() {
        let (engine, _keep) = test_engine(8);

        let (_alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let welcome = next(&mut alice_rx).await;
        let ServerMessage::Welcome { roster, .. } = welcome else {
            panic!("expected welcome, got {welcome:?}");
        };
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].name, "alice");

        let (_bob, mut bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;
        let welcome = next(&mut bob_rx).await;
        let ServerMessage::Welcome { roster, .. } = welcome else {
            panic!("expected welcome, got {welcome:?}");
        };
        assert_eq!(roster.len(), 2, "bob sees alice in the roster");

        let joined = next(&mut alice_rx).await;
        let ServerMessage::ParticipantJoined { participant } = joined else {
            panic!("expected ParticipantJoined, got {joined:?}");
        };
        assert_eq!(participant.name, "bob");
    }

    #[tokio::test]
    async fn malformed_sdp_and_candidates_do_not_wedge_the_engine() {
        let (engine, _keep) = test_engine(8);
        let (alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        next(&mut alice_rx).await; // welcome

        // Garbage SDP offer → the session sees an error, the room survives.
        engine.sdp("ABCD-EFGH-JKLM".into(), alice, "not an sdp offer".into());
        let msg = next(&mut alice_rx).await;
        assert!(matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::InvalidMessage,
                ..
            }
        ));

        // Garbage answer / candidate: ignored without side effects.
        engine.sdp_answer("ABCD-EFGH-JKLM".into(), alice, "junk".into());
        engine.ice("ABCD-EFGH-JKLM".into(), alice, "junk".into());

        // The engine still works for the same session.
        engine.chat("ABCD-EFGH-JKLM".into(), alice, "still here".into());
        let msg = next(&mut alice_rx).await;
        assert!(matches!(msg, ServerMessage::Chat { .. }), "got {msg:?}");
    }

    #[tokio::test]
    async fn duplicate_name_is_rejected() {
        let (engine, _keep) = test_engine(8);
        let (out_tx, _out_rx) = unbounded_channel();
        join(&engine, "ABCD-EFGH-JKLM", "alice").await;

        let reply = engine.join(
            Uuid::new_v4(),
            "ALICE".into(),
            meeting_dto("ABCD-EFGH-JKLM"),
            out_tx,
        );
        assert_eq!(reply.await.unwrap().unwrap_err().code, ErrorCode::NameTaken);
    }

    #[tokio::test]
    async fn capacity_is_enforced() {
        let (engine, _keep) = test_engine(2);
        join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        join(&engine, "ABCD-EFGH-JKLM", "bob").await;

        let (out_tx, _out_rx) = unbounded_channel();
        let reply = engine.join(
            Uuid::new_v4(),
            "carol".into(),
            meeting_dto("ABCD-EFGH-JKLM"),
            out_tx,
        );
        assert_eq!(
            reply.await.unwrap().unwrap_err().code,
            ErrorCode::CapacityReached
        );
    }

    #[tokio::test]
    async fn chat_relays_to_everyone_including_sender() {
        let (engine, _keep) = test_engine(8);
        let (alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let (_bob, mut bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;
        next(&mut bob_rx).await; // bob's welcome
        next(&mut alice_rx).await; // alice's welcome
        next(&mut alice_rx).await; // ParticipantJoined(bob) on alice's side

        engine.chat("ABCD-EFGH-JKLM".into(), alice, "hello world".into());

        for rx in [&mut alice_rx, &mut bob_rx] {
            let msg = next(rx).await;
            let ServerMessage::Chat { message } = msg else {
                panic!("expected chat, got {msg:?}");
            };
            assert_eq!(message.body, "hello world");
            assert_eq!(message.sender_name, "alice");
        }
    }

    #[tokio::test]
    async fn mute_state_broadcasts_to_everyone() {
        let (engine, _keep) = test_engine(8);
        let (alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let (_bob, mut bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;
        next(&mut bob_rx).await; // welcome
        next(&mut alice_rx).await; // welcome
        next(&mut alice_rx).await; // bob joined

        engine.mute("ABCD-EFGH-JKLM".into(), alice, Some(true), None);

        for rx in [&mut alice_rx, &mut bob_rx] {
            let msg = next(rx).await;
            let ServerMessage::MuteState {
                id: target,
                audio_muted,
                video_muted,
            } = msg
            else {
                panic!("expected mute state, got {msg:?}");
            };
            assert_eq!(target, alice);
            assert!(audio_muted);
            assert!(!video_muted);
        }
    }

    #[tokio::test]
    async fn leave_notifies_the_others() {
        let (engine, _keep) = test_engine(8);
        let (alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let (_bob, mut bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;
        next(&mut bob_rx).await; // welcome
        next(&mut alice_rx).await; // welcome
        next(&mut alice_rx).await; // bob joined

        engine.leave("ABCD-EFGH-JKLM".into(), alice);

        let msg = next(&mut bob_rx).await;
        assert!(matches!(
            msg,
            ServerMessage::ParticipantLeft { id } if id == alice
        ));
        // Alice's session queue is dropped by the engine; her channel
        // closes on the next recv.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), alice_rx.recv())
            .await
            .expect("timeout while waiting for channel close");
        assert!(closed.is_none(), "expected channel close, got {closed:?}");
    }
}
