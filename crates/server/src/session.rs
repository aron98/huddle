//! WebSocket sessions: the signaling glue between a browser and the media
//! engine.
//!
//! One session task per connection. It validates the join against the
//! meetings store (does the meeting exist, is its join window open), then
//! registers the participant with the engine and settles into a pump:
//! client frames become engine commands, engine messages become frames.
//! When the socket closes — gracefully or not — the session removes the
//! participant from the engine, which is what fans `ParticipantLeft` out
//! to everyone else.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use huddle_protocol::{ClientMessage, ErrorCode, Joinability, ServerMessage};
use time::OffsetDateTime;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use uuid::Uuid;

use crate::meetings::{self, normalize_code};
use crate::state::AppState;

/// Entry point for the `/ws` route.
pub async fn ws(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| run_session(socket, state))
}

async fn run_session(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // The first frame must be a join; anything else gets one error and a
    // close.
    let (meeting_code, name) = match expect_join(&mut receiver).await {
        JoinOutcome::Joined { meeting_code, name } => (meeting_code, name),
        JoinOutcome::Closed => return,
        JoinOutcome::Rejected { code, message } => {
            send_error(&mut sender, code, message).await;
            return;
        }
    };

    // Validate the meeting + join window, then hand the session to the
    // engine. The engine enforces capacity and name uniqueness; both it
    // and this validation must agree on the meeting's normalized code.
    let (out_tx, mut out_rx) = unbounded_channel::<ServerMessage>();
    let id = match try_join(&state, &meeting_code, &name, out_tx).await {
        Ok(id) => id,
        Err((code, message)) => {
            send_error(&mut sender, code, message).await;
            return;
        }
    };

    let code = normalize_code(&meeting_code);
    let engine = state.engine().clone();

    loop {
        tokio::select! {
            // Engine → socket.
            outbound = out_rx.recv() => {
                let Some(msg) = outbound else { break };
                let Ok(text) = serde_json::to_string(&msg) else { break };
                if sender.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            // Socket → engine.
            frame = receiver.next() => {
                let Some(Ok(frame)) = frame else { break };
                let Message::Text(text) = frame else { continue };
                let Ok(msg) = serde_json::from_str::<ClientMessage>(&text) else {
                    send_error(&mut sender, ErrorCode::InvalidMessage, "malformed message").await;
                    break;
                };
                match msg {
                    ClientMessage::SdpOffer { sdp } => engine.sdp(code.clone(), id, sdp),
                    ClientMessage::SdpAnswer { sdp } => engine.sdp_answer(code.clone(), id, sdp),
                    ClientMessage::IceCandidate { candidate, .. } => {
                        engine.ice(code.clone(), id, candidate);
                    }
                    ClientMessage::Chat { body } => engine.chat(code.clone(), id, body),
                    ClientMessage::MuteState { audio_muted, video_muted } => {
                        engine.mute(code.clone(), id, audio_muted, video_muted);
                    }
                    ClientMessage::Speaking { speaking } => {
                        engine.speaking(code.clone(), id, speaking);
                    }
                    // Screen-share signaling; wired into the engine in a
                    // follow-up commit.
                    ClientMessage::Sharing { .. } => {}
                    ClientMessage::Leave => break,
                    ClientMessage::Join { .. } => {
                        send_error(&mut sender, ErrorCode::InvalidMessage, "already joined").await;
                        break;
                    }
                }
            }
        }
    }

    engine.leave(code, id);
}

/// What the first frame on a fresh socket resolved to.
enum JoinOutcome {
    /// A well-formed join message.
    Joined { meeting_code: String, name: String },
    /// The socket closed before any usable frame arrived.
    Closed,
    /// The frame was wrong; `code`/`message` tell the client why.
    Rejected {
        code: ErrorCode,
        message: &'static str,
    },
}

async fn expect_join(receiver: &mut SplitStream<WebSocket>) -> JoinOutcome {
    let Some(frame) = receiver.next().await else {
        return JoinOutcome::Closed;
    };
    let text = match frame {
        Ok(Message::Text(text)) => text,
        Ok(_) => {
            return JoinOutcome::Rejected {
                code: ErrorCode::InvalidMessage,
                message: "expected a text frame",
            };
        }
        Err(_) => {
            return JoinOutcome::Rejected {
                code: ErrorCode::Internal,
                message: "socket error",
            };
        }
    };
    match serde_json::from_str::<ClientMessage>(&text) {
        Ok(ClientMessage::Join {
            meeting_code, name, ..
        }) => JoinOutcome::Joined { meeting_code, name },
        Ok(_) => JoinOutcome::Rejected {
            code: ErrorCode::InvalidMessage,
            message: "expected a join message",
        },
        Err(_) => JoinOutcome::Rejected {
            code: ErrorCode::InvalidMessage,
            message: "malformed message",
        },
    }
}

/// Validate a join and register the participant with the engine. On
/// success the engine is already sending: `Welcome` sits at the front of
/// the session's outbound queue.
async fn try_join(
    state: &AppState,
    meeting_code: &str,
    name: &str,
    out: UnboundedSender<ServerMessage>,
) -> Result<Uuid, (ErrorCode, String)> {
    let name = name.trim();
    if name.is_empty() {
        return Err((ErrorCode::InvalidMessage, "name must not be empty".into()));
    }

    let meeting = meetings::get_by_code(state.pool(), meeting_code)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "meeting lookup failed");
            (ErrorCode::Internal, "meeting lookup failed".into())
        })?
        .ok_or_else(|| {
            (
                ErrorCode::MeetingNotFound,
                "no meeting with that code".into(),
            )
        })?;

    match meetings::joinability(&meeting, state.config(), OffsetDateTime::now_utc()) {
        Joinability::Open => {}
        Joinability::NotYet { .. } => {
            return Err((
                ErrorCode::MeetingNotYetOpen,
                "the meeting has not started yet".into(),
            ));
        }
        Joinability::Ended => {
            return Err((ErrorCode::MeetingEnded, "the meeting has ended".into()));
        }
        Joinability::Cancelled => {
            return Err((
                ErrorCode::MeetingCancelled,
                "the meeting was cancelled".into(),
            ));
        }
    }

    let id = Uuid::new_v4();
    match state
        .engine()
        .join(id, name.to_owned(), meeting.into(), out)
        .await
    {
        Ok(Ok(())) => Ok(id),
        Ok(Err(rejected)) => Err((rejected.code, join_refused_message(rejected.code).into())),
        Err(_) => Err((
            ErrorCode::Internal,
            "the media engine is unavailable".into(),
        )),
    }
}

fn join_refused_message(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::NameTaken => "that name is already taken in this meeting",
        ErrorCode::CapacityReached => "this meeting is full",
        _ => "join refused",
    }
}

async fn send_error(
    sender: &mut SplitSink<WebSocket, Message>,
    code: ErrorCode,
    message: impl Into<String>,
) {
    let msg = ServerMessage::Error {
        code,
        message: message.into(),
    };
    if let Ok(text) = serde_json::to_string(&msg) {
        let _ = sender.send(Message::Text(text.into())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state::AppState;
    use huddle_protocol::CreateMeetingRequest;
    use std::net::{IpAddr, SocketAddr, UdpSocket};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc as std_mpsc;
    use std::thread;
    use std::time::{Duration, Instant};
    use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
    use str0m::media::{Direction, MediaKind, MediaTime, Mid};
    use str0m::net::{Protocol, Receive};
    use str0m::{Candidate, Event, Input, Output, Rtc};
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    // ------------------------------------------------------------------
    // Test server: real axum router + real engine + real UDP media socket.
    // ------------------------------------------------------------------

    struct TestServer {
        base: String,
        media_addr: SocketAddr,
        state: AppState,
    }

    impl TestServer {
        async fn start() -> Self {
            let config = Config {
                media_port: 0,
                // Tests talk loopback-to-loopback; detection is machine-dependent.
                public_ip: Some(IpAddr::from([127, 0, 0, 1])),
                ..Config::default()
            };
            let pool = crate::db::test_pool().await;
            let (engine, media_addr) = crate::engine::spawn(&config).unwrap();
            let state = AppState::new(Arc::new(config), pool, engine);
            let app = crate::http::router(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                base: format!("ws://{addr}"),
                media_addr,
                state,
            }
        }

        async fn create_meeting(&self, starts_in_min: i64, lasts_min: i64) -> String {
            let now = OffsetDateTime::now_utc();
            let req = CreateMeetingRequest {
                title: "Test call".into(),
                starts_at: now + time::Duration::minutes(starts_in_min),
                ends_at: now + time::Duration::minutes(starts_in_min + lasts_min),
            };
            let meeting = meetings::create(self.state.pool(), &req, now)
                .await
                .unwrap();
            meeting.code.clone()
        }
    }

    // ------------------------------------------------------------------
    // Test client: a WS connection plus a dedicated OS thread that owns a
    // str0m Rtc and a UDP socket, exactly the shape a browser has.
    // ------------------------------------------------------------------

    enum PumpCommand {
        CreateOffer {
            reply: oneshot::Sender<String>,
        },
        AcceptAnswer {
            sdp: String,
        },
        AcceptServerOffer {
            sdp: String,
            reply: oneshot::Sender<String>,
        },
        AddCandidate {
            candidate: String,
        },
        Stop,
    }

    struct PumpHandle {
        cmd: std_mpsc::Sender<PumpCommand>,
        connected: Arc<AtomicBool>,
        media_rx: Arc<AtomicU64>,
    }

    impl PumpHandle {
        fn connected(&self) -> bool {
            self.connected.load(Ordering::Relaxed)
        }
        fn media_rx(&self) -> u64 {
            self.media_rx.load(Ordering::Relaxed)
        }
        fn stop(&self) {
            let _ = self.cmd.send(PumpCommand::Stop);
        }
    }

    /// Spawn the client's "browser thread": real UDP socket, real str0m
    /// RTC, a 20ms audio stream once the offer has been created.
    fn spawn_pump() -> PumpHandle {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(2)))
            .unwrap();
        let local = socket.local_addr().unwrap();
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<PumpCommand>();
        let connected = Arc::new(AtomicBool::new(false));
        let media_rx = Arc::new(AtomicU64::new(0));
        let thread_connected = connected.clone();
        let thread_media_rx = media_rx.clone();
        thread::Builder::new()
            .name("test-client".into())
            .spawn(move || {
                pump_loop(socket, local, cmd_rx, &thread_connected, &thread_media_rx);
            })
            .unwrap();
        PumpHandle {
            cmd: cmd_tx,
            connected,
            media_rx,
        }
    }

    fn pump_loop(
        socket: UdpSocket,
        local: SocketAddr,
        cmds: std_mpsc::Receiver<PumpCommand>,
        connected: &AtomicBool,
        media_rx: &AtomicU64,
    ) {
        let mut rtc = Rtc::builder().build(Instant::now());
        rtc.add_local_candidate(Candidate::host(local, "udp").unwrap())
            .unwrap();

        let mut pending: Option<SdpPendingOffer> = None;
        let mut mid: Option<Mid> = None;
        let mut frame = 0u64;
        let mut next_audio = Instant::now();

        loop {
            // Commands from the async test side.
            while let Ok(cmd) = cmds.try_recv() {
                match cmd {
                    PumpCommand::CreateOffer { reply } => {
                        let mut change = rtc.sdp_api();
                        let m = change.add_media(
                            MediaKind::Audio,
                            Direction::SendRecv,
                            Some("test-stream".into()),
                            None,
                            None,
                        );
                        match change.apply() {
                            Some((offer, p)) => {
                                mid = Some(m);
                                pending = Some(p);
                                let _ = reply.send(offer.to_sdp_string());
                            }
                            None => {
                                let _ = reply.send(String::new());
                            }
                        }
                    }
                    PumpCommand::AcceptAnswer { sdp } => {
                        if let (Some(p), Ok(answer)) =
                            (pending.take(), SdpAnswer::from_sdp_string(&sdp))
                        {
                            rtc.sdp_api().accept_answer(p, answer).ok();
                        }
                    }
                    PumpCommand::AcceptServerOffer { sdp, reply } => {
                        let answer = SdpOffer::from_sdp_string(&sdp)
                            .ok()
                            .and_then(|offer| rtc.sdp_api().accept_offer(offer).ok())
                            .map(|answer| answer.to_sdp_string())
                            .unwrap_or_default();
                        let _ = reply.send(answer);
                    }
                    PumpCommand::AddCandidate { candidate } => {
                        if let Ok(c) = Candidate::from_sdp_string(&candidate) {
                            rtc.add_remote_candidate(c);
                        }
                    }
                    PumpCommand::Stop => return,
                }
            }

            // Socket → RTC.
            let mut buf = [0u8; 2048];
            while let Ok((n, source)) = socket.recv_from(&mut buf) {
                if let Ok(recv) = Receive::new(Protocol::Udp, source, local, &buf[..n]) {
                    rtc.handle_input(Input::Receive(Instant::now(), recv)).ok();
                }
            }

            // RTC → socket.
            while let Ok(out) = rtc.poll_output() {
                match out {
                    Output::Transmit(t) => {
                        socket.send_to(&t.contents, t.destination).ok();
                    }
                    Output::Timeout(_) => break,
                    Output::Event(Event::Connected) => {
                        connected.store(true, Ordering::Relaxed);
                    }
                    Output::Event(Event::MediaData(_)) => {
                        media_rx.fetch_add(1, Ordering::Relaxed);
                    }
                    Output::Event(_) => {}
                }
            }

            // Send audio at a 20ms cadence once the m-line exists.
            if let Some(m) = mid
                && Instant::now() >= next_audio
            {
                let pt = rtc
                    .codec_config()
                    .iter()
                    .find(|p| p.spec().codec == str0m::format::Codec::Opus)
                    .map(|p| p.pt())
                    .unwrap();
                if let Some(writer) = rtc.writer(m) {
                    let time = MediaTime::from_millis(20 * frame);
                    writer
                        .start_of_talkspurt(frame == 0)
                        .write(pt, Instant::now(), time, vec![0x11; 40])
                        .ok();
                }
                frame += 1;
                next_audio = Instant::now() + Duration::from_millis(20);
            }

            rtc.handle_input(Input::Timeout(Instant::now())).ok();
        }
    }

    struct TestClient {
        ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        pump: PumpHandle,
        you: Uuid,
        roster: Vec<huddle_protocol::Participant>,
    }

    impl TestClient {
        async fn connect(server: &TestServer, code: &str, name: &str) -> Self {
            let request = format!("{}/ws", server.base).into_client_request().unwrap();
            let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            let mut ws = ws;
            let pump = spawn_pump();

            let join = serde_json::json!({
                "type": "join",
                "meeting_code": code,
                "name": name,
            });
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                join.to_string().into(),
            ))
            .await
            .unwrap();

            let mut client = Self {
                ws,
                pump,
                you: Uuid::nil(),
                roster: Vec::new(),
            };
            let msg = client.next_server_message().await;
            let ServerMessage::Welcome {
                you,
                roster,
                protocol_version,
                ..
            } = msg
            else {
                panic!("expected welcome, got {msg:?}")
            };
            assert_eq!(protocol_version, huddle_protocol::PROTOCOL_VERSION);
            client.you = you.id;
            client.roster = roster;
            client
        }

        async fn next_server_message(&mut self) -> ServerMessage {
            let frame = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("message within 5s")
                .expect("socket open")
                .expect("frame ok");
            let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
                panic!("expected text frame, got {frame:?}")
            };
            serde_json::from_str(&text).expect("valid server message")
        }

        async fn send_client(&mut self, msg: &ClientMessage) {
            let json = serde_json::to_string(msg).unwrap();
            self.ws
                .send(tokio_tungstenite::tungstenite::Message::Text(json.into()))
                .await
                .unwrap();
        }

        /// Publish: create an audio offer via the pump, send it, and wait
        /// for the SFU's answer.
        async fn publish(&mut self) {
            let (tx, rx) = oneshot::channel();
            self.pump
                .cmd
                .send(PumpCommand::CreateOffer { reply: tx })
                .unwrap();
            let sdp = rx.await.expect("pump alive");
            assert!(!sdp.is_empty(), "pump produced an offer");
            self.send_client(&ClientMessage::SdpOffer { sdp }).await;

            let msg = self.next_server_message().await;
            let ServerMessage::SdpAnswer { sdp } = msg else {
                panic!("expected sdp answer, got {msg:?}")
            };
            self.pump
                .cmd
                .send(PumpCommand::AcceptAnswer { sdp })
                .unwrap();
        }

        /// Route everything that arrives within `wait`: answers get
        /// applied, server offers get answered, candidates get added.
        async fn drain(&mut self, wait: Duration) {
            loop {
                let Ok(frame) = tokio::time::timeout(wait, self.ws.next()).await else {
                    return;
                };
                let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = frame else {
                    return;
                };
                let Ok(msg) = serde_json::from_str::<ServerMessage>(&text) else {
                    return;
                };
                match msg {
                    ServerMessage::SdpAnswer { sdp } => {
                        self.pump
                            .cmd
                            .send(PumpCommand::AcceptAnswer { sdp })
                            .unwrap();
                    }
                    ServerMessage::SdpOffer { sdp } => {
                        let (tx, rx) = oneshot::channel();
                        self.pump
                            .cmd
                            .send(PumpCommand::AcceptServerOffer { sdp, reply: tx })
                            .unwrap();
                        let answer = rx.await.expect("pump alive");
                        assert!(!answer.is_empty(), "pump answered the server offer");
                        self.send_client(&ClientMessage::SdpAnswer { sdp: answer })
                            .await;
                    }
                    ServerMessage::IceCandidate { candidate, .. } => {
                        self.pump
                            .cmd
                            .send(PumpCommand::AddCandidate { candidate })
                            .unwrap();
                    }
                    _ => {}
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // The two-person call.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn two_person_call_streams_media_through_the_server() {
        let server = TestServer::start().await;
        assert_eq!(server.media_addr.ip(), IpAddr::from([127, 0, 0, 1]));
        let code = server.create_meeting(0, 60).await;

        let mut alice = TestClient::connect(&server, &code, "alice").await;
        let mut bob = TestClient::connect(&server, &code, "bob").await;

        // Bob's arrival is visible to alice before either publishes.
        let msg = alice.next_server_message().await;
        let ServerMessage::ParticipantJoined { participant } = msg else {
            panic!("expected ParticipantJoined, got {msg:?}")
        };
        assert_eq!(participant.name, "bob");

        // Both publish; the SFU renegotiates each side to forward the
        // other's track, which arrives as a server-initiated offer.
        alice.publish().await;
        bob.publish().await;

        // Let the call run until both clients are connected AND each has
        // received the other's audio through the SFU.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let settled = alice.pump.connected()
                && bob.pump.connected()
                && alice.pump.media_rx() > 0
                && bob.pump.media_rx() > 0;
            if settled {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "call did not settle: alice connected={} media={}, bob connected={} media={}",
                alice.pump.connected(),
                alice.pump.media_rx(),
                bob.pump.connected(),
                bob.pump.media_rx()
            );
            alice.drain(Duration::from_millis(20)).await;
            bob.drain(Duration::from_millis(20)).await;
        }

        alice.pump.stop();
        bob.pump.stop();
    }

    // ------------------------------------------------------------------
    // Error paths.
    // ------------------------------------------------------------------

    async fn ws_connect(base: &str) -> WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>> {
        let request = format!("{base}/ws").into_client_request().unwrap();
        let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws
    }

    async fn send_json(
        ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        value: serde_json::Value,
    ) {
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            value.to_string().into(),
        ))
        .await
        .unwrap();
    }

    async fn expect_error(
        ws: &mut WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        code: ErrorCode,
    ) {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("message within 5s")
            .expect("socket open")
            .expect("frame ok");
        let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
            panic!("expected text frame, got {frame:?}")
        };
        let msg: ServerMessage = serde_json::from_str(&text).unwrap();
        assert!(
            matches!(msg, ServerMessage::Error { code: c, .. } if c == code),
            "expected error {code:?}, got {msg:?}"
        );
    }

    #[tokio::test]
    async fn join_unknown_meeting_is_rejected() {
        let server = TestServer::start().await;
        let mut ws = ws_connect(&server.base).await;
        send_json(
            &mut ws,
            serde_json::json!({ "type": "join", "meeting_code": "ZZZZ-ZZZZ-ZZZZ", "name": "alice" }),
        )
        .await;
        expect_error(&mut ws, ErrorCode::MeetingNotFound).await;
    }

    #[tokio::test]
    async fn join_before_window_is_rejected() {
        let server = TestServer::start().await;
        let code = server.create_meeting(30, 30).await;
        let mut ws = ws_connect(&server.base).await;
        send_json(
            &mut ws,
            serde_json::json!({ "type": "join", "meeting_code": code, "name": "alice" }),
        )
        .await;
        expect_error(&mut ws, ErrorCode::MeetingNotYetOpen).await;
    }

    #[tokio::test]
    async fn join_after_end_is_rejected() {
        let server = TestServer::start().await;
        let code = server.create_meeting(-60, 30).await;
        let mut ws = ws_connect(&server.base).await;
        send_json(
            &mut ws,
            serde_json::json!({ "type": "join", "meeting_code": code, "name": "alice" }),
        )
        .await;
        expect_error(&mut ws, ErrorCode::MeetingEnded).await;
    }

    #[tokio::test]
    async fn messages_before_join_are_rejected() {
        let server = TestServer::start().await;
        let mut ws = ws_connect(&server.base).await;
        send_json(&mut ws, serde_json::json!({ "type": "chat", "body": "hi" })).await;
        expect_error(&mut ws, ErrorCode::InvalidMessage).await;
    }

    #[tokio::test]
    async fn duplicate_name_is_rejected_over_ws() {
        let server = TestServer::start().await;
        let code = server.create_meeting(0, 60).await;

        let alice = TestClient::connect(&server, &code, "alice").await;
        alice.pump.stop();

        let mut ws = ws_connect(&server.base).await;
        send_json(
            &mut ws,
            serde_json::json!({ "type": "join", "meeting_code": code, "name": "ALICE" }),
        )
        .await;
        expect_error(&mut ws, ErrorCode::NameTaken).await;
    }

    #[tokio::test]
    async fn leave_is_broadcast_to_the_room() {
        let server = TestServer::start().await;
        let code = server.create_meeting(0, 60).await;

        let mut alice = TestClient::connect(&server, &code, "alice").await;
        let mut bob = TestClient::connect(&server, &code, "bob").await;

        // Alice sees bob join, then bob leaves.
        let msg = alice.next_server_message().await;
        assert!(matches!(msg, ServerMessage::ParticipantJoined { .. }));

        bob.send_client(&ClientMessage::Leave).await;
        let msg = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let msg = alice.next_server_message().await;
                match msg {
                    ServerMessage::ParticipantLeft { .. } => break msg,
                    ServerMessage::ParticipantJoined { .. } => continue,
                    other => panic!("unexpected {other:?}"),
                }
            }
        })
        .await
        .expect("left within 5s");
        assert!(matches!(msg, ServerMessage::ParticipantLeft { id } if id == bob.you));

        alice.pump.stop();
        bob.pump.stop();
    }

    #[tokio::test]
    async fn speaking_is_broadcast_to_the_room() {
        let server = TestServer::start().await;
        let code = server.create_meeting(0, 60).await;

        let mut alice = TestClient::connect(&server, &code, "alice").await;
        let mut bob = TestClient::connect(&server, &code, "bob").await;

        // Alice sees bob join first; both then hear alice speak.
        let msg = alice.next_server_message().await;
        assert!(matches!(msg, ServerMessage::ParticipantJoined { .. }));

        alice
            .send_client(&ClientMessage::Speaking { speaking: true })
            .await;

        let expected = alice.you;
        for client in [&mut alice, &mut bob] {
            let msg = client.next_server_message().await;
            let ServerMessage::Speaking { id, speaking } = msg else {
                panic!("expected speaking, got {msg:?}")
            };
            assert_eq!(id, expected);
            assert!(speaking);
        }

        alice.pump.stop();
        bob.pump.stop();
    }

    // Keep unused-import warnings honest: `UnboundedReceiver` is part of
    // the module's public shape (the engine hands us the sending side).
    #[allow(dead_code)]
    fn _type_assertions(_rx: UnboundedReceiver<ServerMessage>) {}
}
