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
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use huddle_protocol::{
    ErrorCode, IceServer, PROTOCOL_VERSION, Participant, ServerMessage, VideoLayer,
};
use huddle_sfu::{RecorderSink, Room, SfuOutput};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use uuid::Uuid;

use serde::Serialize;

use crate::config::Config;
use crate::meetings::normalize_code;

/// Worst-case poll cadence when no peer asked to be woken earlier.
const MAX_SLEEP: Duration = Duration::from_millis(50);

/// Why a join was refused, mirrored from [`ErrorCode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinRejected {
    pub code: ErrorCode,
}

/// Snapshot of the engine's live state, as rendered by `/metrics`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct StatsReport {
    /// One entry per live meeting room.
    pub rooms: Vec<RoomReport>,
}

/// One live meeting room.
#[derive(Debug, Clone, Serialize)]
pub struct RoomReport {
    /// Normalized meeting code.
    pub code: String,
    /// Every participant currently in the room, sorted by name.
    pub peers: Vec<PeerReport>,
}

/// One participant's counters and link stats.
#[derive(Debug, Clone, Serialize)]
pub struct PeerReport {
    pub id: Uuid,
    pub name: String,
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub keyframes_requested: u64,
    pub bytes_rx: u64,
    pub bytes_tx: u64,
    pub bitrate_in_bps: u64,
    pub bitrate_out_bps: u64,
    pub rtt_ms: Option<u64>,
    pub ingress_loss_fraction: Option<f32>,
    pub egress_loss_fraction: Option<f32>,
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
        message: huddle_protocol::ChatMessage,
    },
    /// Update a participant's mute state and broadcast it.
    Mute {
        code: String,
        id: Uuid,
        audio_muted: Option<bool>,
        video_muted: Option<bool>,
    },
    /// Relay a speaking indicator to everyone in the meeting.
    Speaking {
        code: String,
        id: Uuid,
        speaking: bool,
    },
    /// Update a participant's share state and broadcast it.
    Sharing {
        code: String,
        id: Uuid,
        sharing: bool,
    },
    /// Set the simulcast layer a receiver wants from another
    /// participant's camera track.
    LayerPreference {
        code: String,
        id: Uuid,
        origin: Uuid,
        layer: VideoLayer,
    },
    /// Begin recording a meeting with the given sink. Fails with
    /// [`ErrorCode::MeetingNotLive`] when nobody is in the room and
    /// [`ErrorCode::RecordingActive`] when a recording is running.
    StartRecording {
        code: String,
        recorder: Arc<dyn RecorderSink>,
        reply: oneshot::Sender<Result<(), ErrorCode>>,
    },
    /// Stop the recording on a meeting (idempotent; a second stop is
    /// `Ok`). Dropping the sink is what finalizes the files.
    StopRecording {
        code: String,
        reply: oneshot::Sender<Result<(), ErrorCode>>,
    },
    /// Relay an opaque E2EE keying payload from one participant to
    /// another. The payload is ciphertext to this server (ADR 010):
    /// it is never inspected, never persisted, and only delivered to
    /// the addressed participant.
    E2eeKeying {
        code: String,
        from: Uuid,
        to: Uuid,
        epoch: u64,
        payload: String,
    },
    /// Remove a participant (graceful leave or socket gone).
    Leave { code: String, id: Uuid },
    /// Collect a snapshot of rooms and peer stats (the `/metrics` pull).
    Stats { reply: oneshot::Sender<StatsReport> },
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

    pub fn chat(&self, code: String, message: huddle_protocol::ChatMessage) {
        let _ = self.tx.send(EngineCommand::Chat { code, message });
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

    pub fn speaking(&self, code: String, id: Uuid, speaking: bool) {
        let _ = self.tx.send(EngineCommand::Speaking { code, id, speaking });
    }

    pub fn sharing(&self, code: String, id: Uuid, sharing: bool) {
        let _ = self.tx.send(EngineCommand::Sharing { code, id, sharing });
    }

    pub fn layer_preference(&self, code: String, id: Uuid, origin: Uuid, layer: VideoLayer) {
        let _ = self.tx.send(EngineCommand::LayerPreference {
            code,
            id,
            origin,
            layer,
        });
    }

    /// Install a recording sink on the meeting's live room.
    pub fn start_recording(
        &self,
        code: String,
        recorder: Arc<dyn RecorderSink>,
    ) -> oneshot::Receiver<Result<(), ErrorCode>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(EngineCommand::StartRecording {
            code,
            recorder,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Stop the meeting's recording, finalizing the sink's files.
    pub fn stop_recording(&self, code: String) -> oneshot::Receiver<Result<(), ErrorCode>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(EngineCommand::StopRecording {
            code,
            reply: reply_tx,
        });
        reply_rx
    }

    /// Relay an opaque E2EE keying payload to another participant.
    pub fn e2ee_keying(&self, code: String, from: Uuid, to: Uuid, epoch: u64, payload: String) {
        let _ = self.tx.send(EngineCommand::E2eeKeying {
            code,
            from,
            to,
            epoch,
            payload,
        });
    }

    pub fn leave(&self, code: String, id: Uuid) {
        let _ = self.tx.send(EngineCommand::Leave { code, id });
    }

    /// Request a snapshot of rooms and peer stats from the engine.
    pub fn stats(&self) -> oneshot::Receiver<StatsReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.tx.send(EngineCommand::Stats { reply: reply_tx });
        reply_rx
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
    /// The meeting's E2EE trust mode, fixed when the instance is
    /// created from the first joiner's meeting row. Plaintext
    /// meetings keep working exactly as before (ADR 010).
    e2ee: bool,
    /// The current SFrame epoch label. Starts at 0 and increments on
    /// every join and leave so leavers never hold keys for the epochs
    /// that follow them (meeting-scoped forward secrecy). The server
    /// only labels epochs; all key material lives client-side.
    e2ee_epoch: u64,
    /// The recording sink, when a recording is running. Held here AND
    /// inside the room: when the instance is dropped (last leave, or
    /// explicit stop clears it) the last `Arc` goes away, which is
    /// what tears the writer thread down and finalizes the files.
    recorder: Option<Arc<dyn RecorderSink>>,
}

impl MeetingInstance {
    fn new(media_addr: SocketAddr, e2ee: bool) -> Self {
        Self {
            room: Room::new(media_addr),
            participants: HashMap::new(),
            outs: HashMap::new(),
            names: HashSet::new(),
            e2ee,
            e2ee_epoch: 0,
            recorder: None,
        }
    }

    fn recording(&self) -> bool {
        self.recorder.is_some()
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
/// media address, which pairs the configured/detected public ip with it.
pub fn spawn(config: &Config) -> anyhow::Result<(EngineHandle, SocketAddr)> {
    let socket = UdpSocket::bind(SocketAddr::new(
        IpAddr::from([0, 0, 0, 0]),
        config.media_port,
    ))?;
    let bound = socket.local_addr()?;
    let media_addr = SocketAddr::new(config.effective_public_ip(), bound.port());

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
            EngineCommand::Chat { code, message } => self.relay_chat(&code, message),
            EngineCommand::Mute {
                code,
                id,
                audio_muted,
                video_muted,
            } => self.set_mute(&code, id, audio_muted, video_muted),
            EngineCommand::Speaking { code, id, speaking } => {
                self.relay_speaking(&code, id, speaking);
            }
            EngineCommand::Sharing { code, id, sharing } => {
                self.set_sharing(&code, id, sharing);
            }
            EngineCommand::LayerPreference {
                code,
                id,
                origin,
                layer,
            } => {
                self.set_layer_preference(&code, id, origin, layer);
            }
            EngineCommand::StartRecording {
                code,
                recorder,
                reply,
            } => {
                self.start_recording(&code, recorder, reply);
            }
            EngineCommand::StopRecording { code, reply } => {
                self.stop_recording(&code, reply);
            }
            EngineCommand::E2eeKeying {
                code,
                from,
                to,
                epoch,
                payload,
            } => self.relay_e2ee_keying(&code, from, to, epoch, payload),
            EngineCommand::Leave { code, id } => self.remove_participant(&code, id),
            EngineCommand::Stats { reply } => {
                let _ = reply.send(self.collect_stats());
            }
            EngineCommand::Packet { source, bytes } => self.handle_packet(source, &bytes),
            EngineCommand::Shutdown => unreachable!("handled by the run loop"),
        }
    }

    /// One pull of everything `/metrics` renders: rooms, peers, and their
    /// counters. Sorting by code/name keeps successive scrapes readable.
    fn collect_stats(&self) -> StatsReport {
        let mut rooms = Vec::with_capacity(self.meetings.len());
        for (code, inst) in &self.meetings {
            let mut peers: Vec<PeerReport> = inst
                .participants
                .values()
                .map(|p| {
                    let s = inst.room.peer_stats(p.id).unwrap_or_default();
                    PeerReport {
                        id: p.id,
                        name: p.name.clone(),
                        rx_frames: s.rx_frames,
                        tx_frames: s.tx_frames,
                        keyframes_requested: s.keyframes_requested,
                        bytes_rx: s.bytes_rx,
                        bytes_tx: s.bytes_tx,
                        bitrate_in_bps: s.bitrate_in_bps,
                        bitrate_out_bps: s.bitrate_out_bps,
                        rtt_ms: s.rtt_ms,
                        ingress_loss_fraction: s.ingress_loss_fraction,
                        egress_loss_fraction: s.egress_loss_fraction,
                    }
                })
                .collect();
            peers.sort_by(|a, b| a.name.cmp(&b.name));
            rooms.push(RoomReport {
                code: code.clone(),
                peers,
            });
        }
        rooms.sort_by(|a, b| a.code.cmp(&b.code));
        StatsReport { rooms }
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
            .or_insert_with(|| MeetingInstance::new(self.media_addr, meeting.e2ee));
        if let Err(e) = inst.room.create_peer(id) {
            tracing::error!(peer = %id, "create_peer failed: {e}");
            reject(reply, ErrorCode::Internal);
            return;
        }

        // Rotation on join (ADR 010): the joiner never shares an epoch
        // with anyone who left before them, and their own arrival kills
        // every older key. The Welcome below carries the new label so
        // the joiner keys itself for the epoch the room is now in.
        if inst.e2ee {
            inst.e2ee_epoch += 1;
        }
        let (e2ee_enabled, e2ee_epoch) = (inst.e2ee, inst.e2ee_epoch);

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
            recording: inst.recording(),
            e2ee_enabled,
            e2ee_epoch,
        });

        let joined = ServerMessage::ParticipantJoined { participant };
        let rotated = e2ee_enabled.then_some(ServerMessage::E2eeEpoch { epoch: e2ee_epoch });
        for (pid, o) in &inst.outs {
            if *pid != id {
                let _ = o.send(joined.clone());
                if let Some(msg) = &rotated {
                    let _ = o.send(msg.clone());
                }
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

    /// Relay an opaque E2EE keying payload from one participant to
    /// another (ADR 010). The server never sees usable key material:
    /// the payload is AES-GCM ciphertext under a key only the receiver
    /// holds. Relaying outside an E2EE meeting, or from/to someone not
    /// in the room, is a silent drop — a missing receiver usually
    /// just left (their epoch already rotated without them).
    fn relay_e2ee_keying(&mut self, code: &str, from: Uuid, to: Uuid, epoch: u64, payload: String) {
        let Some(inst) = self.meetings.get_mut(code) else {
            return;
        };
        if !inst.e2ee || !inst.participants.contains_key(&from) {
            return;
        }
        let Some(out) = inst.outs.get(&to) else {
            return;
        };
        let _ = out.send(ServerMessage::E2eeKeying {
            from,
            to,
            epoch,
            payload,
        });
    }

    /// Relay a chat message verbatim. The session built it (and persisted
    /// it); the engine only needs to confirm the sender is still here.
    fn relay_chat(&mut self, code: &str, message: huddle_protocol::ChatMessage) {
        let Some(inst) = self.meetings.get_mut(code) else {
            return;
        };
        if let Some(sender) = message.sender_id
            && !inst.participants.contains_key(&sender)
        {
            return;
        }
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
        // Mirror the state into the room so forwarding stops at the SFU,
        // not just in the clients' tiles.
        if let Err(e) = inst.room.set_mute(id, Some(audio_muted), Some(video_muted)) {
            tracing::debug!(peer = %id, "room set_mute: {e}");
        }
        let msg = ServerMessage::MuteState {
            id,
            audio_muted,
            video_muted,
        };
        for out in inst.outs.values() {
            let _ = out.send(msg.clone());
        }
    }

    /// Apply a receiver's simulcast layer preference at the SFU. Layer
    /// choice is per-receiver, so there is nothing to broadcast: the
    /// media plane starts forwarding the chosen rid (plus a throttled
    /// keyframe request on the switched-to layer) on the next frames.
    fn set_layer_preference(&mut self, code: &str, id: Uuid, origin: Uuid, layer: VideoLayer) {
        let Some(inst) = self.meetings.get_mut(code) else {
            return;
        };
        if !inst.participants.contains_key(&id) {
            return;
        }
        if let Err(e) = inst.room.set_layer_preference(id, origin, layer) {
            tracing::debug!(peer = %id, "room set_layer_preference: {e}");
        }
    }

    /// Relay a speaking indicator. The engine does not gate the value in
    /// any way; the client is the authority on its own audio activity.
    fn relay_speaking(&mut self, code: &str, id: Uuid, speaking: bool) {
        let Some(inst) = self.meetings.get_mut(code) else {
            return;
        };
        if !inst.participants.contains_key(&id) {
            return;
        }
        let msg = ServerMessage::Speaking { id, speaking };
        for out in inst.outs.values() {
            let _ = out.send(msg.clone());
        }
    }

    /// Update a participant's share state, mirror it into the room so
    /// the SFU stops/resumes screen forwarding, and tell everyone.
    ///
    /// Single active share: when a new participant starts sharing,
    /// every other current sharer is stood down — their roster flag is
    /// cleared, the SFU stops their screen forwarding immediately, and
    /// they get a directed `Sharing{false}` so their client runs its
    /// normal stop path (drop track, renegotiate, broadcast the wind
    /// down). Viewers learn about the switch through that echo plus
    /// the new sharer's broadcast.
    fn set_sharing(&mut self, code: &str, id: Uuid, sharing: bool) {
        let Some(inst) = self.meetings.get_mut(code) else {
            return;
        };
        let Some(p) = inst.participants.get_mut(&id) else {
            return;
        };
        p.sharing = sharing;
        if let Err(e) = inst.room.set_sharing(id, sharing) {
            tracing::debug!(peer = %id, "room set_sharing: {e}");
        }
        let preempted: Vec<Uuid> = if sharing {
            inst.participants
                .values()
                .filter(|p| p.sharing && p.id != id)
                .map(|p| p.id)
                .collect()
        } else {
            Vec::new()
        };
        for prev in preempted {
            if let Some(p) = inst.participants.get_mut(&prev) {
                p.sharing = false;
            }
            if let Err(e) = inst.room.set_sharing(prev, false) {
                tracing::debug!(peer = %prev, "room set_sharing: {e}");
            }
            if let Some(out) = inst.outs.get(&prev) {
                let _ = out.send(ServerMessage::Sharing {
                    id: prev,
                    sharing: false,
                });
            }
        }
        let msg = ServerMessage::Sharing { id, sharing };
        for out in inst.outs.values() {
            let _ = out.send(msg.clone());
        }
    }

    /// Install a recording sink on a live room and announce it. Nobody
    /// in the room yet → [`ErrorCode::MeetingNotLive`]; already
    /// recording → [`ErrorCode::RecordingActive`]. The sink lives in
    /// the meeting instance (and the room): when the last participant
    /// leaves, the instance — and with it the sink — is dropped, which
    /// stops the recording without anybody asking. Fail-open: nothing
    /// in the media path ever depends on the sink behaving.
    fn start_recording(
        &mut self,
        code: &str,
        recorder: Arc<dyn RecorderSink>,
        reply: oneshot::Sender<Result<(), ErrorCode>>,
    ) {
        let code = normalize_code(code);
        let Some(inst) = self.meetings.get_mut(&code) else {
            let _ = reply.send(Err(ErrorCode::MeetingNotLive));
            return;
        };
        if inst.recorder.is_some() {
            let _ = reply.send(Err(ErrorCode::RecordingActive));
            return;
        }
        // Interlock (ADR 009/010): the engine refuses independently of
        // the REST handler — E2EE meetings only ever relay ciphertext.
        if inst.e2ee {
            let _ = reply.send(Err(ErrorCode::E2eeActive));
            return;
        }
        inst.room.set_recorder(Some(recorder.clone()));
        inst.recorder = Some(recorder);
        let msg = ServerMessage::Recording { active: true };
        for out in inst.outs.values() {
            let _ = out.send(msg.clone());
        }
        let _ = reply.send(Ok(()));
    }

    /// Stop the recording on a live room and announce it. Idempotent:
    /// stopping an unrecorded room is still `Ok`. Stopping a room that
    /// is not live is [`ErrorCode::MeetingNotLive`] — the empty-room
    /// lifecycle already tore the recorder down in that case.
    fn stop_recording(&mut self, code: &str, reply: oneshot::Sender<Result<(), ErrorCode>>) {
        let code = normalize_code(code);
        let Some(inst) = self.meetings.get_mut(&code) else {
            let _ = reply.send(Err(ErrorCode::MeetingNotLive));
            return;
        };
        inst.room.set_recorder(None);
        inst.recorder = None; // dropping the Arc tears down the writer
        let msg = ServerMessage::Recording { active: false };
        for out in inst.outs.values() {
            let _ = out.send(msg.clone());
        }
        let _ = reply.send(Ok(()));
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
            let rotate = inst.e2ee.then(|| {
                // Rotation on leave (ADR 010): every key from the
                // leaver's epochs dies with them. An emptied room
                // takes its epoch counter with the instance.
                inst.e2ee_epoch += 1;
                ServerMessage::E2eeEpoch {
                    epoch: inst.e2ee_epoch,
                }
            });
            for out in inst.outs.values() {
                let _ = out.send(msg.clone());
                if let Some(rotated) = &rotate {
                    let _ = out.send(rotated.clone());
                }
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
            // Tests talk loopback-to-loopback; detection is machine-dependent.
            public_ip: Some(IpAddr::from([127, 0, 0, 1])),
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
            repeat: None,
            has_passcode: false,
            e2ee: false,
            title_ciphertext: None,
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

    fn meeting_dto_e2ee(code: &str) -> MeetingDto {
        let mut meeting = meeting_dto(code);
        meeting.e2ee = true;
        meeting
    }

    /// Like [`join`], but in E2EE trust mode.
    async fn join_e2ee(
        handle: &EngineHandle,
        code: &str,
        name: &str,
    ) -> (Uuid, tokio::sync::mpsc::UnboundedReceiver<ServerMessage>) {
        let id = Uuid::new_v4();
        let (out_tx, out_rx) = unbounded_channel();
        let reply = handle.join(id, name.to_owned(), meeting_dto_e2ee(code), out_tx);
        reply.await.expect("engine alive").expect("join accepted");
        (id, out_rx)
    }

    /// Drain whatever is already queued. Engine replies arrive after
    /// every broadcast of the same command, so once a join's reply
    /// came back the queue holds everything that join produced.
    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerMessage>) {
        while rx.try_recv().is_ok() {}
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
        engine.chat(
            "ABCD-EFGH-JKLM".into(),
            chat_message(alice, "alice", "still here"),
        );
        let msg = next(&mut alice_rx).await;
        assert!(matches!(msg, ServerMessage::Chat { .. }), "got {msg:?}");
    }

    fn chat_message(sender: Uuid, name: &str, body: &str) -> huddle_protocol::ChatMessage {
        huddle_protocol::ChatMessage {
            id: Uuid::new_v4(),
            sender_id: Some(sender),
            sender_name: name.into(),
            body: body.into(),
            sent_at: time::OffsetDateTime::now_utc(),
        }
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

        engine.chat(
            "ABCD-EFGH-JKLM".into(),
            chat_message(alice, "alice", "hello world"),
        );

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
    async fn speaking_broadcasts_to_everyone() {
        let (engine, _keep) = test_engine(8);
        let (alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let (_bob, mut bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;
        next(&mut bob_rx).await; // welcome
        next(&mut alice_rx).await; // welcome
        next(&mut alice_rx).await; // bob joined

        engine.speaking("ABCD-EFGH-JKLM".into(), alice, true);
        engine.speaking("ABCD-EFGH-JKLM".into(), alice, false);

        for speaking in [true, false] {
            for rx in [&mut alice_rx, &mut bob_rx] {
                let msg = next(rx).await;
                let ServerMessage::Speaking { id, speaking: s } = msg else {
                    panic!("expected speaking, got {msg:?}");
                };
                assert_eq!(id, alice);
                assert_eq!(s, speaking);
            }
        }
    }

    #[tokio::test]
    async fn sharing_broadcasts_to_everyone_and_updates_the_roster() {
        let (engine, _keep) = test_engine(8);
        let (alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let (_bob, mut bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;
        next(&mut bob_rx).await; // welcome
        next(&mut alice_rx).await; // welcome
        next(&mut alice_rx).await; // bob joined

        engine.sharing("ABCD-EFGH-JKLM".into(), alice, true);
        engine.sharing("ABCD-EFGH-JKLM".into(), alice, false);

        for sharing in [true, false] {
            for rx in [&mut alice_rx, &mut bob_rx] {
                let msg = next(rx).await;
                let ServerMessage::Sharing { id, sharing: s } = msg else {
                    panic!("expected sharing, got {msg:?}");
                };
                assert_eq!(id, alice);
                assert_eq!(s, sharing);
            }
        }

        // A late joiner's Welcome roster reflects the current share state.
        engine.sharing("ABCD-EFGH-JKLM".into(), alice, true);
        next(&mut alice_rx).await; // echo to alice
        next(&mut bob_rx).await; // relay to bob

        let (_carol, mut carol_rx) = join(&engine, "ABCD-EFGH-JKLM", "carol").await;
        let welcome = next(&mut carol_rx).await;
        let ServerMessage::Welcome { roster, .. } = welcome else {
            panic!("expected welcome, got {welcome:?}");
        };
        let alice_in_roster = roster
            .iter()
            .find(|p| p.name == "alice")
            .expect("alice in roster");
        assert!(alice_in_roster.sharing);
    }

    #[tokio::test]
    async fn second_sharer_preempts_the_first() {
        let (engine, _keep) = test_engine(8);
        let (alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let (bob, mut bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;
        let (_carol, mut carol_rx) = join(&engine, "ABCD-EFGH-JKLM", "carol").await;
        next(&mut carol_rx).await; // welcome
        next(&mut bob_rx).await; // welcome
        next(&mut bob_rx).await; // carol joined
        next(&mut alice_rx).await; // welcome
        next(&mut alice_rx).await; // bob joined
        next(&mut alice_rx).await; // carol joined

        // Alice starts sharing; the echo goes to everyone (incl. her).
        engine.sharing("ABCD-EFGH-JKLM".into(), alice, true);
        for rx in [&mut alice_rx, &mut bob_rx, &mut carol_rx] {
            let msg = next(rx).await;
            let ServerMessage::Sharing { id, sharing: s } = msg else {
                panic!("expected sharing, got {msg:?}");
            };
            assert_eq!(id, alice);
            assert!(s);
        }

        // Bob preempts: alice first gets the directed stand-down, then
        // everyone (incl. alice and bob) hears that bob is sharing.
        engine.sharing("ABCD-EFGH-JKLM".into(), bob, true);
        let msg = next(&mut alice_rx).await;
        let ServerMessage::Sharing { id, sharing: s } = msg else {
            panic!("expected directed stand-down, got {msg:?}");
        };
        assert_eq!(id, alice);
        assert!(!s);
        for rx in [&mut alice_rx, &mut bob_rx, &mut carol_rx] {
            let msg = next(rx).await;
            let ServerMessage::Sharing { id, sharing: s } = msg else {
                panic!("expected sharing, got {msg:?}");
            };
            assert_eq!(id, bob);
            assert!(s);
        }

        // The preempted client runs its stop path; the resulting echo
        // is broadcast so viewers everywhere wind the old share down.
        engine.sharing("ABCD-EFGH-JKLM".into(), alice, false);
        for rx in [&mut alice_rx, &mut bob_rx, &mut carol_rx] {
            let msg = next(rx).await;
            let ServerMessage::Sharing { id, sharing: s } = msg else {
                panic!("expected sharing, got {msg:?}");
            };
            assert_eq!(id, alice);
            assert!(!s);
        }

        // A late joiner sees only bob as the active sharer.
        let (_dave, mut dave_rx) = join(&engine, "ABCD-EFGH-JKLM", "dave").await;
        let welcome = next(&mut dave_rx).await;
        let ServerMessage::Welcome { roster, .. } = welcome else {
            panic!("expected welcome, got {welcome:?}");
        };
        let bob_in_roster = roster
            .iter()
            .find(|p| p.name == "bob")
            .expect("bob in roster");
        assert!(bob_in_roster.sharing);
        let alice_in_roster = roster
            .iter()
            .find(|p| p.name == "alice")
            .expect("alice in roster");
        assert!(!alice_in_roster.sharing);
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

    #[tokio::test]
    async fn stats_report_lists_rooms_and_peers() {
        let (engine, _keep) = test_engine(8);
        let (_alice, _alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let (_bob, _bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;

        let report = engine.stats().await.expect("engine alive");
        assert_eq!(report.rooms.len(), 1);
        assert_eq!(report.rooms[0].code, "ABCD-EFGH-JKLM");
        // Peers are sorted by name for stable scrapes.
        assert_eq!(
            report.rooms[0]
                .peers
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alice", "bob"]
        );

        engine.leave("ABCD-EFGH-JKLM".into(), _alice);
        engine.leave("ABCD-EFGH-JKLM".into(), _bob);

        let report = engine.stats().await.expect("engine alive");
        assert!(
            report.rooms.is_empty(),
            "room should be gone after everyone left: {report:?}"
        );
    }

    // --- recording lifecycle ----------------------------------------------

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use huddle_sfu::{RecordedFrame, RecorderSink};

    /// A sink that only reports when its last reference went away —
    /// which is exactly the auto-stop-on-empty signal.
    #[derive(Default)]
    struct DropSpy {
        dropped: Arc<AtomicBool>,
    }

    impl RecorderSink for DropSpy {
        fn record(&self, _frame: RecordedFrame) {}
    }

    impl Drop for DropSpy {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn recording_broadcasts_state_and_late_joiner_sees_the_flag() {
        let (engine, _keep) = test_engine(8);
        let (_alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let (_bob, mut bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;
        next(&mut alice_rx).await; // alice welcome
        next(&mut bob_rx).await; // bob welcome
        next(&mut alice_rx).await; // bob joined

        // Start: everyone in the room is told at once.
        engine
            .start_recording("ABCD-EFGH-JKLM".into(), Arc::new(DropSpy::default()))
            .await
            .expect("engine alive")
            .expect("start accepted");
        for rx in [&mut alice_rx, &mut bob_rx] {
            let msg = next(rx).await;
            let ServerMessage::Recording { active } = msg else {
                panic!("expected recording, got {msg:?}");
            };
            assert!(active);
        }

        // One recording at a time.
        let second = engine
            .start_recording("ABCD-EFGH-JKLM".into(), Arc::new(DropSpy::default()))
            .await
            .expect("engine alive");
        assert_eq!(second.unwrap_err(), ErrorCode::RecordingActive);

        // A late joiner's Welcome announces the recording.
        let (_carol, mut carol_rx) = join(&engine, "ABCD-EFGH-JKLM", "carol").await;
        let welcome = next(&mut carol_rx).await;
        let ServerMessage::Welcome { recording, .. } = welcome else {
            panic!("expected welcome, got {welcome:?}");
        };
        assert!(recording, "late joiner must know the meeting is recorded");
        next(&mut alice_rx).await; // carol joined
        next(&mut bob_rx).await; // carol joined

        // Stop: the state is broadcast to everyone, including carol.
        engine
            .stop_recording("ABCD-EFGH-JKLM".into())
            .await
            .expect("engine alive")
            .expect("stop accepted");
        for rx in [&mut alice_rx, &mut bob_rx, &mut carol_rx] {
            let msg = next(rx).await;
            let ServerMessage::Recording { active } = msg else {
                panic!("expected recording, got {msg:?}");
            };
            assert!(!active);
        }

        // Stopping again is fine (idempotent); stopping a meeting that
        // is not live is a `MeetingNotLive` refusal.
        engine
            .stop_recording("ABCD-EFGH-JKLM".into())
            .await
            .expect("engine alive")
            .expect("second stop accepted");
        let refused = engine
            .stop_recording("ZZZZ-ZZZZ-ZZZZ".into())
            .await
            .expect("engine alive");
        assert_eq!(refused.unwrap_err(), ErrorCode::MeetingNotLive);
    }

    #[tokio::test]
    async fn recording_stops_when_the_meeting_empties() {
        let (engine, _keep) = test_engine(8);
        let (alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        next(&mut alice_rx).await; // welcome

        let spy = Arc::new(DropSpy::default());
        let dropped = spy.dropped.clone();
        engine
            .start_recording("ABCD-EFGH-JKLM".into(), spy)
            .await
            .expect("engine alive")
            .expect("start accepted");
        next(&mut alice_rx).await; // recording started

        // The last leave must tear the recorder down all by itself.
        engine.leave("ABCD-EFGH-JKLM".into(), alice);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !dropped.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            dropped.load(Ordering::SeqCst),
            "the sink must be dropped when the room empties"
        );

        // The room is gone; recording it again is refused.
        let refused = engine
            .start_recording("ABCD-EFGH-JKLM".into(), Arc::new(DropSpy::default()))
            .await
            .expect("engine alive");
        assert_eq!(refused.unwrap_err(), ErrorCode::MeetingNotLive);
    }

    // --- e2ee keying relay (issue #27) --------------------------------------

    #[tokio::test]
    async fn e2ee_welcome_carries_the_trust_mode_and_epoch() {
        let (engine, _keep) = test_engine(8);
        let (_alice, mut alice_rx) = join_e2ee(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let welcome = next(&mut alice_rx).await;
        let ServerMessage::Welcome {
            e2ee_enabled,
            e2ee_epoch,
            ..
        } = welcome
        else {
            panic!("expected welcome, got {welcome:?}");
        };
        assert!(e2ee_enabled, "the joiner learns the trust mode");
        assert_eq!(e2ee_epoch, 1, "the first join rotates the epoch from 0");
    }

    #[tokio::test]
    async fn plaintext_meetings_stay_rotation_free() {
        let (engine, _keep) = test_engine(8);
        let (_alice, mut alice_rx) = join(&engine, "ABCD-EFGH-JKLM", "alice").await;
        let welcome = next(&mut alice_rx).await;
        let ServerMessage::Welcome {
            e2ee_enabled,
            e2ee_epoch,
            ..
        } = welcome
        else {
            panic!("expected welcome, got {welcome:?}");
        };
        assert!(!e2ee_enabled);
        assert_eq!(e2ee_epoch, 0);

        let (_bob, mut bob_rx) = join(&engine, "ABCD-EFGH-JKLM", "bob").await;
        next(&mut bob_rx).await; // bob welcome
        next(&mut alice_rx).await; // bob joined — nothing else follows
        assert!(
            matches!(
                alice_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "plaintext joins do not broadcast epochs"
        );
    }

    #[tokio::test]
    async fn e2ee_epochs_rotate_on_join_and_leave() {
        let (engine, _keep) = test_engine(8);
        let code = "ABCD-EFGH-JKLM";
        let (_alice, mut alice_rx) = join_e2ee(&engine, code, "alice").await;
        next(&mut alice_rx).await; // welcome, epoch 1

        let (bob, mut bob_rx) = join_e2ee(&engine, code, "bob").await;
        let welcome = next(&mut bob_rx).await;
        let ServerMessage::Welcome { e2ee_epoch, .. } = welcome else {
            panic!("expected welcome, got {welcome:?}");
        };
        assert_eq!(e2ee_epoch, 2, "the joiner keys itself for the new epoch");
        next(&mut alice_rx).await; // bob joined
        let rotated = next(&mut alice_rx).await;
        let ServerMessage::E2eeEpoch { epoch } = rotated else {
            panic!("expected epoch broadcast, got {rotated:?}");
        };
        assert_eq!(epoch, 2, "everyone else rotates with the joiner");

        engine.leave(code.into(), bob);
        next(&mut alice_rx).await; // bob left
        let rotated = next(&mut alice_rx).await;
        let ServerMessage::E2eeEpoch { epoch } = rotated else {
            panic!("expected epoch broadcast, got {rotated:?}");
        };
        assert_eq!(epoch, 3, "leavers never hold keys for what follows");
    }

    #[tokio::test]
    async fn e2ee_keying_relays_only_to_the_addressee() {
        let (engine, _keep) = test_engine(8);
        let code = "ABCD-EFGH-JKLM";
        let (alice, mut alice_rx) = join_e2ee(&engine, code, "alice").await;
        let (bob, mut bob_rx) = join_e2ee(&engine, code, "bob").await;
        let (_carol, mut carol_rx) = join_e2ee(&engine, code, "carol").await;
        drain(&mut alice_rx);
        drain(&mut bob_rx);
        drain(&mut carol_rx);

        engine.e2ee_keying(code.into(), alice, bob, 3, "opaque".into());
        let msg = next(&mut bob_rx).await;
        let ServerMessage::E2eeKeying {
            from,
            to,
            epoch,
            payload,
        } = msg
        else {
            panic!("expected keying relay, got {msg:?}");
        };
        assert_eq!(from, alice);
        assert_eq!(to, bob);
        assert_eq!(epoch, 3);
        assert_eq!(payload, "opaque");
        for rx in [&mut carol_rx, &mut alice_rx] {
            assert!(
                matches!(
                    rx.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ),
                "keying is directed: nobody else sees it"
            );
        }

        // A receiver who already left is a silent drop, not an error.
        engine.leave(code.into(), bob);
        next(&mut alice_rx).await; // bob left
        next(&mut alice_rx).await; // the epoch rotated
        engine.e2ee_keying(code.into(), alice, bob, 4, "late".into());
        assert!(
            matches!(
                alice_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "no echo for the sender, no delivery for the gone"
        );
    }

    #[tokio::test]
    async fn e2ee_keying_is_ignored_outside_e2ee_meetings() {
        let (engine, _keep) = test_engine(8);
        let code = "ABCD-EFGH-JKLM";
        let (alice, mut alice_rx) = join(&engine, code, "alice").await;
        let (bob, mut bob_rx) = join(&engine, code, "bob").await;
        drain(&mut alice_rx);
        drain(&mut bob_rx);

        // The trust mode is off: keying is meaningless, so it is
        // dropped without a delivery or an error.
        engine.e2ee_keying(code.into(), alice, bob, 0, "nope".into());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            matches!(
                bob_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "plaintext meetings never relay keying"
        );

        // A sender who is not in the room cannot key anyone either.
        let (carol, mut carol_rx) = join_e2ee(&engine, "MNOP-QRST-UVWX", "carol").await;
        drain(&mut carol_rx);
        engine.e2ee_keying(
            "MNOP-QRST-UVWX".into(),
            Uuid::new_v4(),
            carol,
            1,
            "x".into(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            matches!(
                carol_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "unknown senders cannot key anyone"
        );
    }

    #[tokio::test]
    async fn recording_is_refused_in_e2ee_meetings() {
        let (engine, _keep) = test_engine(8);
        let (_alice, _alice_rx) = join_e2ee(&engine, "ABCD-EFGH-JKLM", "alice").await;

        let refused = engine
            .start_recording("ABCD-EFGH-JKLM".into(), Arc::new(DropSpy::default()))
            .await
            .expect("engine alive");
        assert_eq!(refused.unwrap_err(), ErrorCode::E2eeActive);
    }
}
