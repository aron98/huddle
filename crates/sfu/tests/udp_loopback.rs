//! Real-socket integration: the room's media plane over actual UDP
//! loopback sockets, proving datagram demultiplexing (`accepts`) and the
//! embedder contract (poll → send → recv → feed → timeout).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use huddle_sfu::{PeerId, Room, SfuOutput};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

/// Size of the receive buffers; str0m warns above ~1500 bytes.
const BUF_LEN: usize = 2000;

/// Wall-clock budget for drive loops before giving up.
const DRIVE_BUDGET: Duration = Duration::from_secs(15);

/// Pause between drive iterations so the OS can move datagrams around.
const ITERATION_PAUSE: Duration = Duration::from_millis(1);

/// A synthetic browser with a real UDP socket.
struct TestClient {
    id: PeerId,
    socket: UdpSocket,
    addr: SocketAddr,
    rtc: Rtc,
    pending: Option<str0m::change::SdpPendingOffer>,
    offer: String,
    mid: str0m::media::Mid,
    connected: bool,
    media_rx: usize,
}

impl TestClient {
    fn new() -> TestClient {
        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_opus(true, false)
            .enable_vp8(true)
            .build(Instant::now());
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let addr = socket.local_addr().unwrap();
        rtc.add_local_candidate(Candidate::host(addr, "udp").unwrap());

        let mut changes = rtc.sdp_api();
        let mid = changes.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = changes.apply().unwrap();

        TestClient {
            id: PeerId::new_v4(),
            socket,
            addr,
            rtc,
            pending: Some(pending),
            offer: offer.to_sdp_string(),
            mid,
            connected: false,
            media_rx: 0,
        }
    }

    /// Complete the initial offer/answer exchange against the room.
    fn join(&mut self, room: &mut Room) {
        room.create_peer(self.id).unwrap();
        let answer = room.answer_sdp_offer(self.id, &self.offer).unwrap();
        let pending = self.pending.take().unwrap();
        let answer = SdpAnswer::from_sdp_string(&answer).unwrap();
        self.rtc.sdp_api().accept_answer(pending, answer).unwrap();
    }

    /// Answer a renegotiation offer the room sent us (signaling is
    /// simulated in-process; only media goes over the wire).
    fn handle_server_offer(&mut self, room: &mut Room, sdp: &str) {
        let offer = SdpOffer::from_sdp_string(sdp).unwrap();
        let answer = self.rtc.sdp_api().accept_offer(offer).unwrap();
        room.accept_sdp_answer(self.id, &answer.to_sdp_string())
            .unwrap();
    }

    /// Send `n` opus frames on the client's own m-line.
    fn send_audio(&mut self, n: usize) {
        let pt = self
            .rtc
            .codec_config()
            .iter()
            .find(|p| p.spec().codec == Codec::Opus)
            .unwrap()
            .pt();
        for i in 0..n {
            let time = MediaTime::from_millis(20 * i as u64);
            let data = vec![0x11_u8; 40];
            self.rtc
                .writer(self.mid)
                .unwrap()
                .write(pt, Instant::now(), time, data)
                .unwrap();
        }
    }

    /// Drain the client socket into its `Rtc`.
    fn recv_pending(&mut self) {
        let mut buf = [0_u8; BUF_LEN];
        while let Ok((n, source)) = self.socket.recv_from(&mut buf) {
            let receive = Receive::new(Protocol::Udp, source, self.addr, &buf[..n]).unwrap();
            self.rtc
                .handle_input(Input::Receive(Instant::now(), receive))
                .unwrap();
        }
    }

    /// Drain the `Rtc` outputs onto the socket, tracking events.
    fn pump_outputs(&mut self) {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    self.socket.send_to(&t.contents, t.destination).unwrap();
                }
                Ok(Output::Timeout(_)) => break,
                Ok(Output::Event(e)) => match e {
                    Event::Connected => self.connected = true,
                    Event::MediaData(_) => self.media_rx += 1,
                    _ => {}
                },
                Err(_) => break,
            }
        }
        let _ = self.rtc.handle_input(Input::Timeout(Instant::now()));
    }
}

/// The server's side of the embedder contract over a real socket.
struct MediaSocket {
    socket: UdpSocket,
    addr: SocketAddr,
}

impl MediaSocket {
    fn new() -> MediaSocket {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let addr = socket.local_addr().unwrap();
        MediaSocket { socket, addr }
    }

    /// Drain the socket into the room (demultiplexing per peer).
    fn recv_pending(&self, room: &mut Room) {
        let mut buf = [0_u8; BUF_LEN];
        while let Ok((n, source)) = self.socket.recv_from(&mut buf) {
            let receive = Receive::new(Protocol::Udp, source, self.addr, &buf[..n]).unwrap();
            room.handle_input(Input::Receive(Instant::now(), receive));
        }
    }

    /// Poll the room and send everything it wants to transmit.
    fn pump_room(&self, room: &mut Room, clients: &mut [&mut TestClient]) {
        for id in room.peer_ids() {
            loop {
                match room.poll_output(id) {
                    Some(SfuOutput::Transmit {
                        destination,
                        contents,
                        ..
                    }) => {
                        self.socket.send_to(&contents, destination).unwrap();
                    }
                    Some(SfuOutput::SdpOffer { peer, sdp }) => {
                        if let Some(client) = clients.iter_mut().find(|c| c.id == peer) {
                            client.handle_server_offer(room, &sdp);
                        }
                    }
                    Some(SfuOutput::Timeout(_)) | None => break,
                    Some(SfuOutput::PeerConnected(_)) | Some(SfuOutput::PeerGone(_)) => {}
                }
            }
        }
        room.handle_timeout(Instant::now());
    }
}

fn drive(
    media: &MediaSocket,
    room: &mut Room,
    clients: &mut [&mut TestClient],
    done: impl Fn(&Room, &[&mut TestClient]) -> bool,
) {
    let start = Instant::now();
    while start.elapsed() < DRIVE_BUDGET {
        media.recv_pending(room);
        media.pump_room(room, clients);
        for client in clients.iter_mut() {
            client.recv_pending();
            client.pump_outputs();
        }
        std::thread::sleep(ITERATION_PAUSE);
        if done(room, clients) {
            return;
        }
    }
    panic!("drive loop timed out after {DRIVE_BUDGET:?}");
}

#[test]
fn udp_two_clients_exchange_media() {
    let media = MediaSocket::new();
    let mut room = Room::new(media.addr);

    let mut c1 = TestClient::new();
    let mut c2 = TestClient::new();
    let ids = [c1.id, c2.id];
    c1.join(&mut room);
    c2.join(&mut room);

    drive(&media, &mut room, &mut [&mut c1, &mut c2], |_, clients| {
        clients.iter().all(|c| c.connected)
    });

    c1.send_audio(20);
    c2.send_audio(20);

    drive(&media, &mut room, &mut [&mut c1, &mut c2], |room, _| {
        ids.iter().all(|&id| {
            room.peer_stats(id)
                .is_some_and(|s| s.rx_frames >= 5 && s.tx_frames >= 5)
        })
    });

    assert!(c1.media_rx > 0, "c1 received nothing over udp");
    assert!(c2.media_rx > 0, "c2 received nothing over udp");
}
