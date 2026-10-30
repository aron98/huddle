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
        repeat: None,
        has_passcode: false,
        e2ee: false,
        title_ciphertext: None,
    }
}

struct MockState {
    received: Arc<Mutex<Vec<ClientMessage>>>,
    error_on_join: bool,
    /// Put a renegotiation offer on the wire before the answer to our own
    /// offer — the shape of a stale server offer racing our negotiation.
    offer_before_answer: bool,
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
                    // A TURN relay, as a real server configured with
                    // coturn would advertise: the client must apply it
                    // to its peer connection.
                    ice_servers: vec![huddle_protocol::IceServer {
                        urls: vec!["turn:turn.example.com:3478".into()],
                        username: Some("huddle".into()),
                        credential: Some("secret".into()),
                    }],
                    recording: false,
                    e2ee_enabled: false,
                    e2ee_epoch: 0,
                }]
            }
            ClientMessage::SdpOffer { .. } => {
                let mut replies = Vec::new();
                if self.offer_before_answer {
                    replies.push(ServerMessage::SdpOffer {
                        sdp: "v=0 stale server offer".into(),
                    });
                }
                replies.push(ServerMessage::SdpAnswer {
                    sdp: "v=0 mock answer".into(),
                });
                // A trickled candidate, as a real SFU sends right after the
                // answer.
                replies.push(ServerMessage::IceCandidate {
                    candidate: "candidate:1 1 udp 2130706431 192.168.1.2 54321 typ host".into(),
                    sdp_mid: Some("0".into()),
                    sdp_m_line_index: Some(0),
                });
                replies
            }
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
            // Screen-share state, echoed back like the server does,
            // attributed to a fixture participant.
            ClientMessage::Sharing { sharing } => vec![ServerMessage::Sharing {
                id: ALICE_ID,
                sharing: *sharing,
            }],
            // Layer preferences and focus hints are consumed by the SFU
            // with no reply.
            ClientMessage::LayerPreference { .. }
            | ClientMessage::Focus { .. }
            // SDP answers and leaves close out flows that already got
            // their replies elsewhere.
            | ClientMessage::SdpAnswer { .. }
            | ClientMessage::Leave => vec![],
            // The mock relays keying like the real server does, then
            // rotates the epoch as a membership change would — the
            // client must track both (issue #27).
            ClientMessage::E2eeKeying { to, epoch, payload } => vec![
                ServerMessage::E2eeKeying {
                    from: ALICE_ID,
                    to: *to,
                    epoch: *epoch,
                    payload: payload.clone(),
                },
                ServerMessage::E2eeEpoch { epoch: *epoch + 1 },
            ],
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
        Self::start_with(error_on_join, false).await
    }

    async fn start_with(error_on_join: bool, offer_before_answer: bool) -> Self {
        let state = Arc::new(MockState {
            received: Arc::new(Mutex::new(Vec::new())),
            error_on_join,
            offer_before_answer,
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
    screen_tracks: usize,
    ice_server_updates: usize,
    /// Make `apply_remote_offer` fail, as Chrome does for an offer whose
    /// m-line order no longer matches the negotiated session.
    fail_remote_offers: bool,
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
        if self.fail_remote_offers {
            return Err(PeerError::Failed(
                "m-line order doesn't match the previous offer/answer".to_owned(),
            ));
        }
        Ok("v=0 fake answer".to_owned())
    }

    async fn add_remote_candidate(&mut self, candidate: LocalCandidate) -> Result<(), PeerError> {
        self.remote_candidates.push(candidate);
        Ok(())
    }

    fn take_local_candidates(&mut self) -> Vec<LocalCandidate> {
        std::mem::take(&mut self.pending_local_candidates)
    }

    fn update_ice_servers(
        &mut self,
        ice_servers: &[huddle_protocol::IceServer],
    ) -> Result<(), PeerError> {
        if !ice_servers.is_empty() {
            self.ice_server_updates += 1;
        }
        Ok(())
    }

    async fn add_screen_track(&mut self) -> Result<(), PeerError> {
        self.screen_tracks += 1;
        Ok(())
    }

    async fn remove_screen_track(&mut self) -> Result<(), PeerError> {
        self.screen_tracks = self.screen_tracks.saturating_sub(1);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Login sign-in method probe (issue #99)
// ---------------------------------------------------------------------------

/// Serve `GET /api/auth/oidc/status` the way the real server does, in
/// one of the two deployment modes: `oidc` mirrors an SSO-configured
/// instance (status plus the public issuer — the client secret never
/// crosses the wire), otherwise the plain magic-link answer. Returns
/// the bound address and the shutdown sender.
async fn start_probe_server(oidc: bool) -> (SocketAddr, oneshot::Sender<()>) {
    let app = Router::new().route(
        "/api/auth/oidc/status",
        get(move || async move {
            let body = if oidc {
                r#"{"status":"oidc_configured","issuer":"https://idp.example.com"}"#
            } else {
                r#"{"status":"oidc_not_configured"}"#
            };
            ([("content-type", "application/json")], body)
        }),
    );
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
    (addr, shutdown_tx)
}

/// One raw `GET` over a real socket — the login page's probe as a
/// wire-level round trip. Returns the status code and the body, no
/// HTTP client crate needed.
async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: huddle.test\r\nConnection: close\r\n\r\n");
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("HTTP status line");
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
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
    client.join("H7KD-PM2X-QR4T", "bob", None).await.unwrap();
    assert_eq!(client.state(), huddle_web::rtc::SessionState::InCall);
    assert_eq!(client.you().map(|p| p.name.as_str()), Some("bob"));
    assert_eq!(client.roster().len(), 2, "alice + bob in welcome roster");
    assert_eq!(
        client.meeting().map(|m| m.code.as_str()),
        Some("H7KD-PM2X-QR4T")
    );
    // The Welcome's TURN server was applied to the peer connection
    // before any offer was created.
    assert_eq!(
        client.peer_mut().ice_server_updates,
        1,
        "welcome ice servers applied at join"
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

    // --- Screen share --------------------------------------------------------
    client.start_screen_share().await.unwrap();
    // The share must be announced before the renegotiation offer.
    {
        let log = server.received.lock().unwrap();
        let share_at = log
            .iter()
            .position(|m| matches!(m, ClientMessage::Sharing { sharing: true }))
            .expect("sharing start was sent");
        assert!(
            log.iter()
                .skip(share_at + 1)
                .any(|m| matches!(m, ClientMessage::SdpOffer { .. })),
            "a renegotiation offer follows the share announcement"
        );
    }
    assert_eq!(client.peer_mut().screen_tracks, 1, "screen track attached");
    // The echoed Sharing travels with the renegotiation exchange; publish's
    // inner loop applies it to the roster.
    assert!(
        client
            .roster()
            .iter()
            .any(|p| p.id == ALICE_ID && p.sharing),
        "roster reflects the share"
    );

    client.stop_screen_share().await.unwrap();
    assert!(
        server
            .saw(|m| matches!(m, ClientMessage::Sharing { sharing: false }))
            .await,
        "server should see the share stop"
    );
    assert_eq!(client.peer_mut().screen_tracks, 0, "screen track detached");
    assert!(
        !client
            .roster()
            .iter()
            .any(|p| p.id == ALICE_ID && p.sharing),
        "roster reflects the stop"
    );

    // --- Focus hint ------------------------------------------------------------
    client.set_focus(Some(ALICE_ID)).await.unwrap();
    assert!(
        server
            .saw(|m| matches!(m, ClientMessage::Focus { origin: Some(id) } if *id == ALICE_ID))
            .await,
        "server should see the focus hint"
    );
    client.set_focus(None).await.unwrap();
    assert!(
        server
            .saw(|m| matches!(m, ClientMessage::Focus { origin: None }))
            .await,
        "server should see the focus cleared"
    );

    // --- Leave ----------------------------------------------------------------
    client.leave().await.unwrap();
    assert!(
        server.saw(|m| matches!(m, ClientMessage::Leave)).await,
        "graceful leave must reach the server"
    );
}

#[tokio::test]
async fn publish_survives_a_stale_deferred_server_offer() {
    // A share that preempts another one puts the SFU's stop-offer for the
    // old share on the wire while our own renegotiation is in flight; the
    // SFU discards that offer when it accepts ours and re-offers from the
    // rebased session. The stale offer must be dropped, not fatal.
    let server = MockServer::start_with(false, true).await;
    let mut client = connect(&server).await;
    client.join("H7KD-PM2X-QR4T", "bob", None).await.unwrap();

    // The peer connection rejects the stale offer, as Chrome does.
    client.peer_mut().fail_remote_offers = true;
    client.publish().await.unwrap();

    // The stale offer was attempted (and dropped) …
    assert_eq!(
        client.peer_mut().remote_offers_applied,
        vec!["v=0 stale server offer"]
    );
    // … our own answer still applied …
    assert_eq!(client.peer_mut().answers_applied, vec!["v=0 mock answer"]);
    // … and no answer was sent for the dropped offer: the SFU regenerates
    // it, and that fresh one will be answered instead.
    assert!(
        !server
            .saw(|m| matches!(
                m,
                ClientMessage::SdpAnswer { sdp } if sdp == "v=0 fake answer"
            ))
            .await
    );
}

#[tokio::test]
async fn join_error_surfaces_server_code() {
    let server = MockServer::start(true).await;
    let mut client = connect(&server).await;

    let err = client.join("NOPE", "bob", None).await.unwrap_err();
    let huddle_web::rtc::RtcError::Server(server_err) = &err else {
        panic!("expected server error, got: {err}");
    };
    assert_eq!(server_err.code, ErrorCode::MeetingNotFound);
    assert_eq!(client.state(), huddle_web::rtc::SessionState::Closed);
}

#[tokio::test]
async fn e2ee_keying_round_trips_and_epochs_track() {
    let server = MockServer::start(false).await;
    let mut client = connect(&server).await;
    client.join("H7KD-PM2X-QR4T", "bob", None).await.unwrap();

    client
        .send_e2ee_keying(ALICE_ID, 7, "envelope".into())
        .await
        .unwrap();
    assert!(
        server
            .saw(|m| matches!(
                m,
                ClientMessage::E2eeKeying { to, epoch, payload }
                    if *to == ALICE_ID && *epoch == 7 && payload == "envelope"
            ))
            .await,
        "server should see the keying envelope"
    );

    // The relayed envelope comes back from the sender, then the epoch
    // rotates — the driver must track the new value.
    match client.next_server_message().await.unwrap() {
        ServerMessage::E2eeKeying {
            from,
            epoch,
            payload,
            ..
        } => {
            assert_eq!(from, ALICE_ID);
            assert_eq!(epoch, 7);
            assert_eq!(payload, "envelope");
        }
        other => panic!("expected relayed keying, got {other:?}"),
    }
    match client.next_server_message().await.unwrap() {
        ServerMessage::E2eeEpoch { epoch } => assert_eq!(epoch, 8),
        other => panic!("expected an epoch rotation, got {other:?}"),
    }
    assert_eq!(client.e2ee_epoch(), 8);

    // Both handled messages were buffered for the e2ee module as well
    // (that is how they survive publish's inner pump loop).
    let drained = client.take_pending_e2ee();
    assert!(
        matches!(
            &drained[..],
            [ServerMessage::E2eeKeying { payload, .. }, ServerMessage::E2eeEpoch { epoch: 8 }]
                if payload == "envelope"
        ),
        "handled keying and rotations are both buffered: {drained:?}"
    );

    // The driver buffers relayed envelopes for the e2ee module instead
    // of dropping them — they must survive publish's inner pump loop.
    client
        .send_e2ee_keying(CAROL_ID, 8, "second".into())
        .await
        .unwrap();
    let first = client.read_server_message().await.unwrap();
    client.handle_server_message(first.clone()).await.unwrap();
    match first {
        ServerMessage::E2eeKeying { payload, .. } => assert_eq!(payload, "second"),
        other => panic!("expected relayed keying, got {other:?}"),
    }
    let buffered = client.take_pending_e2ee();
    assert!(
        matches!(&buffered[..], [ServerMessage::E2eeKeying { payload, .. }] if payload == "second"),
        "the driver buffers keying for the e2ee module: {buffered:?}"
    );
}

// --- login sign-in method probe (issue #99) ------------------------------

#[tokio::test]
async fn login_page_probes_the_sign_in_method_before_any_form_submission() {
    use huddle_web::auth_mode;

    // An SSO-configured server: the probe answer hands the login page
    // the SSO entry, and the browser is sent straight to the IdP — no
    // magic-link form is typed into and refused first (issue #99).
    let (addr, shutdown) = start_probe_server(true).await;
    let (status, body) = http_get(addr, "/api/auth/oidc/status").await;
    assert_eq!(status, 200);
    assert_eq!(
        auth_mode::sso_entry_from_probe(status, &body),
        Some("/api/auth/oidc/login")
    );
    shutdown.send(()).unwrap();

    // A magic-link server: the probe answer keeps the login form
    // completely unchanged — the mode nobody configured an IdP for
    // must not behave any differently.
    let (addr, shutdown) = start_probe_server(false).await;
    let (status, body) = http_get(addr, "/api/auth/oidc/status").await;
    assert_eq!(status, 200);
    assert_eq!(auth_mode::sso_entry_from_probe(status, &body), None);
    shutdown.send(()).unwrap();
}
