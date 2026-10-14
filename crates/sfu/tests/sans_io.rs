//! Sans-io integration: synthetic `str0m` clients wired to a [`Room`]
//! entirely in memory. No sockets — client transmits are fed straight back
//! into the room as `Input::Receive` and vice versa.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use huddle_sfu::{PeerId, Room, SfuError, SfuOutput};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

/// The server's (fake) media address handed to the room.
const MEDIA_ADDR: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 40000));

/// Wall-clock budget for drive loops before giving up.
const DRIVE_BUDGET: Duration = Duration::from_secs(10);

/// A synthetic browser: full `Rtc`, one sendrecv audio m-line, candidates
/// inline in its offer.
struct TestClient {
    id: PeerId,
    addr: SocketAddr,
    rtc: Rtc,
    pending: Option<str0m::change::SdpPendingOffer>,
    offer: String,
    mid: str0m::media::Mid,
    connected: bool,
    media_rx: usize,
}

impl TestClient {
    fn new(n: u16) -> TestClient {
        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_opus(true, false)
            .enable_vp8(true)
            .build(Instant::now());
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 4000 + n));
        rtc.add_local_candidate(Candidate::host(addr, "udp").unwrap());

        let mut changes = rtc.sdp_api();
        let mid = changes.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = changes.apply().unwrap();

        TestClient {
            id: PeerId::new_v4(),
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

    /// Answer a renegotiation offer the room sent us.
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

    fn feed(&mut self, source: SocketAddr, contents: &[u8]) {
        let receive = Receive::new(Protocol::Udp, source, self.addr, contents).unwrap();
        self.rtc
            .handle_input(Input::Receive(Instant::now(), receive))
            .unwrap();
    }
}

/// Pump the room and the clients until `done` holds or the budget expires.
fn drive(
    room: &mut Room,
    clients: &mut [&mut TestClient],
    done: impl Fn(&Room, &[&mut TestClient]) -> bool,
) {
    let start = Instant::now();
    while start.elapsed() < DRIVE_BUDGET {
        step(room, clients);
        if done(room, clients) {
            return;
        }
    }
    panic!("drive loop timed out after {DRIVE_BUDGET:?}");
}

/// One iteration: poll the room, wire its outputs to clients, poll the
/// clients, wire their outputs back, advance time everywhere.
fn step(room: &mut Room, clients: &mut [&mut TestClient]) {
    let now = Instant::now();

    // Room -> clients.
    for id in room.peer_ids() {
        loop {
            match room.poll_output(id) {
                Some(SfuOutput::Transmit {
                    destination,
                    contents,
                    ..
                }) => {
                    if let Some(client) = clients.iter_mut().find(|c| c.addr == destination) {
                        client.feed(MEDIA_ADDR, &contents);
                    }
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

    // Clients -> room.
    for client in clients.iter_mut() {
        loop {
            match client.rtc.poll_output() {
                Ok(Output::Transmit(t)) => {
                    let receive =
                        Receive::new(Protocol::Udp, client.addr, MEDIA_ADDR, &t.contents).unwrap();
                    room.handle_input(Input::Receive(Instant::now(), receive));
                }
                Ok(Output::Timeout(_)) => break,
                Ok(Output::Event(e)) => match e {
                    Event::Connected => client.connected = true,
                    Event::MediaData(_) => client.media_rx += 1,
                    _ => {}
                },
                Err(_) => break,
            }
        }
        let _ = client.rtc.handle_input(Input::Timeout(now));
    }

    room.handle_timeout(now);
}

#[test]
fn single_client_connects_and_streams() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut client = TestClient::new(1);
    let client_id = client.id;
    client.join(&mut room);

    drive(&mut room, &mut [&mut client], |_, clients| {
        clients[0].connected
    });
    assert!(client.connected);

    client.send_audio(10);

    drive(&mut room, &mut [&mut client], |room, _| {
        room.peer_stats(client_id).is_some_and(|s| s.rx_frames >= 5)
    });

    let stats = room.peer_stats(client_id).unwrap();
    assert!(stats.rx_frames >= 5, "sfu received {stats:?}");
}

#[test]
fn two_clients_exchange_media_both_ways() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut c1 = TestClient::new(1);
    let mut c2 = TestClient::new(2);
    let ids = [c1.id, c2.id];
    c1.join(&mut room);
    c2.join(&mut room);

    drive(&mut room, &mut [&mut c1, &mut c2], |_, clients| {
        clients.iter().all(|c| c.connected)
    });

    c1.send_audio(10);
    c2.send_audio(10);

    drive(&mut room, &mut [&mut c1, &mut c2], |room, _| {
        ids.iter().all(|&id| {
            room.peer_stats(id)
                .is_some_and(|s| s.rx_frames >= 5 && s.tx_frames >= 5)
        })
    });

    assert!(c1.media_rx > 0, "c1 received nothing");
    assert!(c2.media_rx > 0, "c2 received nothing");
}

#[test]
fn departed_peers_tracks_are_stopped_and_dropped() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let mut b = TestClient::new(2);
    let mut c = TestClient::new(3);
    let (a_id, b_id, c_id) = (a.id, b.id, c.id);
    a.join(&mut room);
    b.join(&mut room);
    c.join(&mut room);

    drive(&mut room, &mut [&mut a, &mut b, &mut c], |_, clients| {
        clients.iter().all(|x| x.connected)
    });
    a.send_audio(10);
    b.send_audio(10);
    c.send_audio(10);
    drive(&mut room, &mut [&mut a, &mut b, &mut c], |room, _| {
        [a_id, b_id, c_id].iter().all(|&id| {
            room.peer_stats(id)
                .is_some_and(|s| s.rx_frames >= 5 && s.tx_frames >= 5)
        })
    });

    // Full mesh: a forwards to both b and c.
    let (_, a_out) = room.peer_tracks(a_id).unwrap();
    assert_eq!(a_out, 2, "a should forward to b and c");

    // c departs. The survivors must renegotiate the dead m-lines away.
    room.remove_peer(c_id);
    assert!(room.peer_tracks(c_id).is_none());

    drive(&mut room, &mut [&mut a, &mut b], |room, _| {
        let (_, a_out) = room.peer_tracks(a_id).unwrap();
        let (_, b_out) = room.peer_tracks(b_id).unwrap();
        a_out == 1 && b_out == 1
    });

    // Media still flows between the survivors.
    let (wm_a, wm_b) = (a.media_rx, b.media_rx);
    a.send_audio(10);
    b.send_audio(10);
    drive(&mut room, &mut [&mut a, &mut b], |_, clients| {
        clients[0].media_rx > wm_a && clients[1].media_rx > wm_b
    });
}

#[test]
fn departure_during_negotiation_is_swept_after_the_answer_lands() {
    // A race the room must survive: the renegotiation ADD offer for c's
    // track is already in a's hands when c departs. The answer converts
    // the just-added line, and the post-answer sweep must wind it back
    // down instead of leaving a zombie forwarding entry.
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let mut c = TestClient::new(2);
    let a_id = a.id;
    a.join(&mut room);
    c.join(&mut room);

    // Drive until the room hands a the add offer, but hold it: a must
    // not answer before c is gone. Both directions are pumped (like
    // `step`) so ICE completes and the room sees c's track arrive.
    let held = {
        let mut held = None;
        let start = Instant::now();
        while held.is_none() && start.elapsed() < DRIVE_BUDGET {
            let now = Instant::now();
            for id in room.peer_ids() {
                loop {
                    match room.poll_output(id) {
                        Some(SfuOutput::Transmit {
                            destination,
                            contents,
                            ..
                        }) => {
                            for client in [&mut a, &mut c] {
                                if client.addr == destination {
                                    client.feed(MEDIA_ADDR, &contents);
                                }
                            }
                        }
                        Some(SfuOutput::SdpOffer { peer, sdp }) => {
                            if peer == a_id {
                                held = Some(sdp);
                                break;
                            }
                            // The newcomer's own add offer is business as
                            // usual: let c answer it.
                            c.handle_server_offer(&mut room, &sdp);
                        }
                        Some(SfuOutput::Timeout(_)) | None => break,
                        Some(SfuOutput::PeerConnected(_)) | Some(SfuOutput::PeerGone(_)) => {}
                    }
                }
            }
            for client in [&mut a, &mut c] {
                loop {
                    match client.rtc.poll_output() {
                        Ok(Output::Transmit(t)) => {
                            let receive =
                                Receive::new(Protocol::Udp, client.addr, MEDIA_ADDR, &t.contents)
                                    .unwrap();
                            room.handle_input(Input::Receive(Instant::now(), receive));
                        }
                        Ok(Output::Timeout(_)) => break,
                        Ok(Output::Event(e)) => {
                            if let Event::Connected = e {
                                client.connected = true;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = client.rtc.handle_input(Input::Timeout(now));
            }
            room.handle_timeout(now);
        }
        held.expect("renegotiation offer for a")
    };

    // c departs with the offer still unanswered.
    room.remove_peer(c.id);

    // Now the answer lands: the swept entry turns into a stop and is
    // renegotiated away entirely.
    a.handle_server_offer(&mut room, &held);
    assert_eq!(room.peer_tracks(a_id).unwrap().1, 1, "zombie entry");

    drive(&mut room, &mut [&mut a], |room, _| {
        room.peer_tracks(a_id).unwrap().1 == 0
    });
    assert!(a.connected, "a must survive the churn");
}

#[test]
fn new_subscribers_get_a_keyframe_from_the_sources() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let mut b = TestClient::new(2);
    let mut c = TestClient::new(3);
    a.join(&mut room);
    b.join(&mut room);
    c.join(&mut room);

    drive(&mut room, &mut [&mut a, &mut b, &mut c], |_, clients| {
        clients.iter().all(|x| x.connected)
    });

    a.send_audio(10);
    b.send_audio(10);
    c.send_audio(10);

    // Drive until everyone is receiving AND every origin has been asked for
    // a keyframe: each client subscribes to two tracks, and each
    // subscription settles with a FIR to the track's origin.
    let ids = [a.id, b.id, c.id];
    drive(&mut room, &mut [&mut a, &mut b, &mut c], |room, clients| {
        let all_have_media = clients.iter().all(|x| x.media_rx > 0);
        let all_were_asked = ids
            .iter()
            .all(|id| room.peer_stats(*id).unwrap().keyframes_requested >= 1);
        all_have_media && all_were_asked
    });

    // Every source the mesh pulled in got a keyframe request over RTCP.
    // (Asserted SFU-side: the wire form is SRTCP, so it is not visible to
    // a sniffer on the receive path.)
    for id in ids {
        assert!(
            room.peer_stats(id).unwrap().keyframes_requested >= 1,
            "{id} was never asked for a keyframe"
        );
    }
}

#[test]
fn room_api_errors() {
    let mut room = Room::new(MEDIA_ADDR);
    let id = PeerId::new_v4();

    // Unknown peer.
    assert!(matches!(
        room.answer_sdp_offer(id, "v=0"),
        Err(SfuError::UnknownPeer)
    ));

    // Duplicate creation.
    room.create_peer(id).unwrap();
    assert!(matches!(room.create_peer(id), Err(SfuError::PeerExists)));

    // Answer without a pending offer.
    assert!(matches!(
        room.accept_sdp_answer(id, "v=0"),
        Err(SfuError::NoPendingOffer)
    ));

    // Garbage ICE candidate.
    assert!(matches!(
        room.add_remote_candidate(id, "not a candidate"),
        Err(SfuError::InvalidCandidate(_))
    ));

    // Garbage SDP.
    assert!(matches!(
        room.answer_sdp_offer(id, "not an sdp"),
        Err(SfuError::InvalidSdp(_))
    ));
}
