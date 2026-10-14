#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration test for the signaling driver: `RtcClient` runs against a
//! tiny mock signaling server (axum WS) over a real socket, with a fake peer
//! connection standing in for the browser's `RTCPeerConnection`.
//!
//! This is the phase-1 acceptance test for the web track: the client joins
//! and round-trips an SDP offer to the mock server.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as ServerFrame, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use huddle_protocol::{
    ChatMessage, ClientMessage, ErrorCode, MeetingDto, PROTOCOL_VERSION, Participant,
    ParticipantId, ServerMessage,
};
use huddle_web::rtc::{LocalCandidate, PeerConnection, PeerError, RtcClient, SignalingStream};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Mock signaling server
// ---------------------------------------------------------------------------

/// Distinct fixed ids so roster updates are individually addressable.
const ALICE_ID: Uuid = Uuid::nil();
const CAROL_ID: Uuid = Uuid::max();

fn participant(id: ParticipantId, name: &str) -> Participant {
    Participant {
        id,
        name: name.into(),
        audio_muted: false,
        video_muted: false,
        sharing: false,
    }
}

fn meeting_dto(code: &str) -> MeetingDto {
    let now = OffsetDateTime::now_utc();
    MeetingDto {
        id: Uuid::new_v4(),
        code: code.into(),
        title: "Mock meeting".into(),
        starts_at: now,
        ends_at: now + time::Duration::hours(1),
        cancelled: false,
    }
}

struct MockState {
    received: Arc<Mutex<Vec<ClientMessage>>>,
    error_on_join: bool,
}

impl MockState {
    fn respond(&self, msg: &ClientMessage) -> Vec<ServerMessage> {
        match msg {
            ClientMessage::Join {
                name, meeting_code, ..
            } => {
                if self.error_on_join {
                    return vec![ServerMessage::Error {
                        code: ErrorCode::MeetingNotFound,
                        message: format!("no meeting {meeting_code}"),
                    }];
                }
                let you = participant(Uuid::new_v4(), name);
                vec![ServerMessage::Welcome {
                    protocol_version: PROTOCOL_VERSION,
                    you: you.clone(),
                    meeting: meeting_dto(meeting_code),
                    // Someone else is already in the room.
                    roster: vec![participant(ALICE_ID, "alice"), you],
                    ice_servers: vec![],
                }]
            }
            ClientMessage::SdpOffer { .. } => vec![
                ServerMessage::SdpAnswer {
                    sdp: "v=0 mock answer".into(),
                },
                // A trickled candidate, as a real SFU sends right after the
                // answer.
                ServerMessage::IceCandidate {
                    candidate: "candidate:1 1 udp 2130706431 192.168.1.2 54321 typ host".into(),
                    sdp_mid: Some("0".into()),
                    sdp_m_line_index: Some(0),
                },
            ],
            // Our trickled candidate reaching the server coincides with a
            // roster change, as it would in a live room.
            ClientMessage::IceCandidate { .. } => vec![ServerMessage::ParticipantJoined {
                participant: participant(CAROL_ID, "carol"),
            }],
            ClientMessage::Chat { body } => vec![
                ServerMessage::Chat {
                    message: ChatMessage {
                        id: Uuid::new_v4(),
                        sender_id: None,
                        sender_name: "mock".into(),
                        body: body.clone(),
                        sent_at: OffsetDateTime::now_utc(),
                    },
                },
                // Renegotiation: the SFU adds a forwarded track.
                ServerMessage::SdpOffer {
                    sdp: "v=0 mock server offer".into(),
                },
            ],
            ClientMessage::MuteState { .. } => vec![ServerMessage::MuteState {
                id: ALICE_ID,
                audio_muted: true,
                video_muted: false,
            }],
            // Speaking is echoed back like the server does, attributed to
            // a fixture participant.
            ClientMessage::Speaking { speaking } => vec![ServerMessage::Speaking {
                id: ALICE_ID,
                speaking: *speaking,
            }],
            // Screen-share state; scripted in a follow-up commit.
            ClientMessage::Sharing { sharing } => vec![ServerMessage::Sharing {
                id: ALICE_ID,
                sharing: *sharing,
            }],
            ClientMessage::SdpAnswer { .. } | ClientMessage::Leave => vec![],
        }
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<MockState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<MockState>) {
    while let Some(Ok(frame)) = socket.recv().await {
        let ServerFrame::Text(text) = frame else {
            continue;
        };
        let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) else {
            continue;
        };
        state.received.lock().unwrap().push(client_msg.clone());
        for reply in state.respond(&client_msg) {
            let encoded = serde_json::to_string(&reply).unwrap();
            if socket
                .send(ServerFrame::Text(encoded.into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

struct MockServer {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<ClientMessage>>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl MockServer {
    async fn start(error_on_join: bool) -> Self {
        let state = Arc::new(MockState {
            received: Arc::new(Mutex::new(Vec::new())),
            error_on_join,
        });
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        Self {
            addr,
            received: state.received.clone(),
            shutdown: Some(shutdown_tx),
        }
    }

    /// Poll (yielding, so the server task can run) until `pred` matches a
    /// recorded message.
    async fn saw<F: Fn(&ClientMessage) -> bool>(&self, pred: F) -> bool {
        for _ in 0..200 {
            if self.received.lock().unwrap().iter().any(&pred) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.received.lock().unwrap().iter().any(&pred)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// Native SignalingStream over tokio-tungstenite
// ---------------------------------------------------------------------------

struct TungsteniteStream {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl SignalingStream for TungsteniteStream {
    async fn send(&mut self, msg: ClientMessage) -> Result<(), huddle_web::rtc::SignalingError> {
        let text = serde_json::to_string(&msg).expect("serialize client message");
        self.socket
            .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
            .await
            .map_err(|e| huddle_web::rtc::SignalingError::Socket(e.to_string()))
    }

    async fn next_message(
        &mut self,
    ) -> Option<Result<ServerMessage, huddle_web::rtc::SignalingError>> {
        loop {
            match self.socket.next().await? {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                    return Some(
                        serde_json::from_str(&text)
                            .map_err(|e| huddle_web::rtc::SignalingError::Malformed(e.to_string())),
                    );
                }
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => return None,
                Ok(_) => continue,
                Err(e) => {
                    return Some(Err(huddle_web::rtc::SignalingError::Socket(e.to_string())));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fake peer connection
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakePeerConnection {
    offers_created: Vec<String>,
    answers_applied: Vec<String>,
    remote_offers_applied: Vec<String>,
    remote_candidates: Vec<LocalCandidate>,
    pending_local_candidates: Vec<LocalCandidate>,
}

impl FakePeerConnection {
    fn with_local_candidate(candidate: LocalCandidate) -> Self {
        Self {
            pending_local_candidates: vec![candidate],
            ..Self::default()
        }
    }
}

impl PeerConnection for FakePeerConnection {
    async fn create_offer(&mut self) -> Result<String, PeerError> {
        let offer = "v=0 fake offer".to_owned();
        self.offers_created.push(offer.clone());
        Ok(offer)
    }

    async fn apply_answer(&mut self, sdp: String) -> Result<(), PeerError> {
        self.answers_applied.push(sdp);
        Ok(())
    }

    async fn apply_remote_offer(&mut self, sdp: String) -> Result<String, PeerError> {
        self.remote_offers_applied.push(sdp);
        Ok("v=0 fake answer".to_owned())
    }

    async fn add_remote_candidate(&mut self, candidate: LocalCandidate) -> Result<(), PeerError> {
        self.remote_candidates.push(candidate);
        Ok(())
    }

    fn take_local_candidates(&mut self) -> Vec<LocalCandidate> {
        std::mem::take(&mut self.pending_local_candidates)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn connect_args(server: &MockServer) -> String {
    format!("ws://{}/ws", server.addr)
}

async fn connect(server: &MockServer) -> RtcClient<TungsteniteStream, FakePeerConnection> {
    let request = connect_args(server).into_client_request().unwrap();
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let peer = FakePeerConnection::with_local_candidate(LocalCandidate {
        candidate: "candidate:1 1 udp 2130706431 192.168.1.9 40000 typ host".into(),
        sdp_mid: Some("0".into()),
        sdp_m_line_index: Some(0),
    });
    RtcClient::new(TungsteniteStream { socket }, peer)
}

#[tokio::test]
async fn join_publish_chat_and_renegotiation_round_trip() {
    let server = MockServer::start(false).await;
    let mut client = connect(&server).await;

    // --- Join ---------------------------------------------------------------
    client.join("H7KD-PM2X-QR4T", "bob").await.unwrap();
    assert_eq!(client.state(), huddle_web::rtc::SessionState::InCall);
    assert_eq!(client.you().map(|p| p.name.as_str()), Some("bob"));
    assert_eq!(client.roster().len(), 2, "alice + bob in welcome roster");
    assert_eq!(
        client.meeting().map(|m| m.code.as_str()),
        Some("H7KD-PM2X-QR4T")
    );

    // --- Publish: offer out, answer back, candidate applied ------------------
    client.publish().await.unwrap();
    assert!(
        server
            .saw(|m| matches!(m, ClientMessage::SdpOffer { sdp } if sdp == "v=0 fake offer"))
            .await,
        "server should see our offer"
    );
    assert_eq!(client.peer_mut().answers_applied, vec!["v=0 mock answer"]);

    // Our gathered ICE candidate was trickled to the server.
    assert!(
        server
            .saw(|m| matches!(
                m,
                ClientMessage::IceCandidate { candidate, .. } if candidate.contains("192.168.1.9")
            ))
            .await,
        "server should see our trickled candidate"
    );

    // The SFU's own candidate (sent after the answer) is applied locally.
    assert!(
        server
            .saw(|m| matches!(m, ClientMessage::IceCandidate { .. }))
            .await
    );
    match client.next_server_message().await.unwrap() {
        ServerMessage::IceCandidate { .. } => {}
        other => panic!("expected sfu candidate, got {other:?}"),
    }
    assert_eq!(
        client.peer_mut().remote_candidates.len(),
        1,
        "sfu candidate applied to the peer connection"
    );

    // --- Roster updates -----------------------------------------------------
    match client.next_server_message().await.unwrap() {
        ServerMessage::ParticipantJoined { participant } => {
            assert_eq!(participant.name, "carol");
        }
        other => panic!("expected roster join, got {other:?}"),
    }
    assert_eq!(client.roster().len(), 3, "alice + bob + carol");

    // --- Chat + server-initiated renegotiation ------------------------------
    client.send_chat("hello".into()).await.unwrap();
    match client.next_server_message().await.unwrap() {
        ServerMessage::Chat { message } => assert_eq!(message.body, "hello"),
        other => panic!("expected chat relay, got {other:?}"),
    }
    assert_eq!(client.chat_log().len(), 1);

    match client.next_server_message().await.unwrap() {
        ServerMessage::SdpOffer { sdp } => assert_eq!(sdp, "v=0 mock server offer"),
        other => panic!("expected server offer, got {other:?}"),
    }
    assert!(
        server
            .saw(|m| matches!(m, ClientMessage::SdpAnswer { sdp } if sdp == "v=0 fake answer"))
            .await,
        "client must answer the server-initiated offer"
    );
    assert_eq!(
        client.peer_mut().remote_offers_applied,
        vec!["v=0 mock server offer"]
    );

    // --- Mute state ----------------------------------------------------------
    client.set_mute_state(Some(true), None).await.unwrap();
    match client.next_server_message().await.unwrap() {
        ServerMessage::MuteState {
            id, audio_muted, ..
        } => {
            assert_eq!(id, ALICE_ID);
            assert!(audio_muted);
        }
        other => panic!("expected mute state, got {other:?}"),
    }
    assert!(client.roster().iter().any(|p| p.audio_muted));

    // --- Speaking -------------------------------------------------------------
    client.set_speaking(true).await.unwrap();
    match client.next_server_message().await.unwrap() {
        ServerMessage::Speaking { id, speaking } => {
            assert_eq!(id, ALICE_ID);
            assert!(speaking);
        }
        other => panic!("expected speaking, got {other:?}"),
    }
    assert!(client.speaking().contains(&ALICE_ID), "alice is speaking");

    // --- Leave ----------------------------------------------------------------
    client.leave().await.unwrap();
    assert!(
        server.saw(|m| matches!(m, ClientMessage::Leave)).await,
        "graceful leave must reach the server"
    );
}

#[tokio::test]
async fn join_error_surfaces_server_code() {
    let server = MockServer::start(true).await;
    let mut client = connect(&server).await;

    let err = client.join("NOPE", "bob").await.unwrap_err();
    let huddle_web::rtc::RtcError::Server(server_err) = &err else {
        panic!("expected server error, got: {err}");
    };
    assert_eq!(server_err.code, ErrorCode::MeetingNotFound);
    assert_eq!(client.state(), huddle_web::rtc::SessionState::Closed);
}
