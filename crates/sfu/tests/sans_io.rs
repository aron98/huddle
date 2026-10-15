//! Sans-io integration: synthetic `str0m` clients wired to a [`Room`]
//! entirely in memory. No sockets — client transmits are fed straight back
//! into the room as `Input::Receive` and vice versa.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use huddle_protocol::VideoLayer;
use huddle_sfu::{PeerId, Room, SfuError, SfuOutput};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Rid, Simulcast, SimulcastLayer};
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
    /// Frames received, keyed by the first payload byte. Simulcast tests
    /// tag each layer's frames with a distinct marker byte to assert
    /// per-subscriber routing.
    media_rx_markers: HashMap<u8, usize>,
    /// Every renegotiation offer the room sent us (for msid assertions).
    server_offers: Vec<String>,
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
            media_rx_markers: HashMap::new(),
            server_offers: Vec::new(),
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
        self.server_offers.push(sdp.to_owned());
        let offer = SdpOffer::from_sdp_string(sdp).unwrap();
        let answer = self.rtc.sdp_api().accept_offer(offer).unwrap();
        room.accept_sdp_answer(self.id, &answer.to_sdp_string())
            .unwrap();
    }

    /// Add a second (video) m-line — the screen track — and offer it to
    /// the room, as a browser does after getDisplayMedia + addTrack.
    fn share_screen(&mut self, room: &mut Room) -> str0m::media::Mid {
        let mut changes = self.rtc.sdp_api();
        let mid = changes.add_media(MediaKind::Video, Direction::SendRecv, None, None, None);
        let (offer, pending) = changes.apply().unwrap();
        let answer = room
            .answer_sdp_offer(self.id, &offer.to_sdp_string())
            .unwrap();
        let answer = SdpAnswer::from_sdp_string(&answer).unwrap();
        self.rtc.sdp_api().accept_answer(pending, answer).unwrap();
        mid
    }

    /// Add a simulcast camera m-line with three send layers ("high",
    /// "mid", "low" in the sender's encoding order, highest quality
    /// first) and offer it to the room, as a browser configured with
    /// RtpSender encodings does.
    fn publish_simulcast_camera(&mut self, room: &mut Room) -> str0m::media::Mid {
        let mut simulcast = Simulcast::new();
        simulcast.add_send_layer(SimulcastLayer::new("high"));
        simulcast.add_send_layer(SimulcastLayer::new("mid"));
        simulcast.add_send_layer(SimulcastLayer::new("low"));
        let mut changes = self.rtc.sdp_api();
        let mid = changes.add_media(
            MediaKind::Video,
            Direction::SendRecv,
            None,
            None,
            Some(simulcast),
        );
        let (offer, pending) = changes.apply().unwrap();
        let answer = room
            .answer_sdp_offer(self.id, &offer.to_sdp_string())
            .unwrap();
        let answer = SdpAnswer::from_sdp_string(&answer).unwrap();
        self.rtc.sdp_api().accept_answer(pending, answer).unwrap();
        mid
    }

    /// Send `n` vp8 frames on one simulcast layer (`rid`), every payload
    /// starting with `marker` so the receiver can tell layers apart.
    fn send_simulcast_frames(&mut self, mid: str0m::media::Mid, rid: &str, marker: u8, n: usize) {
        let pt = self
            .rtc
            .codec_config()
            .iter()
            .find(|p| p.spec().codec == Codec::Vp8)
            .unwrap()
            .pt();
        for i in 0..n {
            let time = MediaTime::from_millis(20 * i as u64);
            let data = vec![marker; 120];
            self.rtc
                .writer(mid)
                .unwrap()
                .rid(Rid::from(rid))
                .write(pt, Instant::now(), time, data)
                .unwrap();
        }
    }

    /// Flip the screen m-line's direction, as removeTrack (stop) and a
    /// re-share (resume) do in a browser.
    fn set_screen_direction(&mut self, room: &mut Room, mid: str0m::media::Mid, dir: Direction) {
        let mut changes = self.rtc.sdp_api();
        changes.set_direction(mid, dir);
        let (offer, pending) = changes.apply().unwrap();
        let answer = room
            .answer_sdp_offer(self.id, &offer.to_sdp_string())
            .unwrap();
        let answer = SdpAnswer::from_sdp_string(&answer).unwrap();
        self.rtc.sdp_api().accept_answer(pending, answer).unwrap();
    }

    /// Send `n` vp8 frames on the screen m-line.
    fn send_screen_frames(&mut self, mid: str0m::media::Mid, n: usize) {
        let pt = self
            .rtc
            .codec_config()
            .iter()
            .find(|p| p.spec().codec == Codec::Vp8)
            .unwrap()
            .pt();
        for i in 0..n {
            let time = MediaTime::from_millis(20 * i as u64);
            let data = vec![0x22_u8; 120];
            self.rtc
                .writer(mid)
                .unwrap()
                .write(pt, Instant::now(), time, data)
                .unwrap();
        }
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

/// Pump for a fixed wall-clock window. str0m paces RTP emission against
/// real time, so frames written now only surface over time — strict
/// count assertions need a settled window. Windows over one second also
/// age the SFU's 1s keyframe-request throttle, making layer-switch
/// keyframe counts deterministic.
fn drain(room: &mut Room, clients: &mut [&mut TestClient], window: Duration) {
    let start = Instant::now();
    while start.elapsed() < window {
        step(room, clients);
    }
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
                    Event::MediaData(d) => {
                        client.media_rx += 1;
                        if let Some(&marker) = d.data.first() {
                            *client.media_rx_markers.entry(marker).or_insert(0) += 1;
                        }
                    }
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
fn peer_stats_capture_link_snapshots() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let a_id = a.id;
    a.join(&mut room);

    drive(&mut room, &mut [&mut a], |_, clients| clients[0].connected);

    // Keep media flowing across several stats windows: the first
    // snapshot only establishes the byte baseline (no predecessor to
    // diff against), so a bitrate only shows up from the second window
    // on — fed here by a continuous trickle of small batches.
    let deadline = Instant::now() + Duration::from_millis(3500);
    while Instant::now() < deadline {
        a.send_audio(5);
        step(&mut room, &mut [&mut a]);
        if room.peer_stats(a_id).is_some_and(|s| s.bitrate_in_bps > 0) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let stats = room.peer_stats(a_id).unwrap();
    assert!(stats.bytes_rx > 0, "no media bytes recorded: {stats:?}");
    assert!(
        stats.bitrate_in_bps > 0,
        "no ingress bitrate computed: {stats:?}"
    );
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
fn muted_sources_stop_forwarding_until_unmuted() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let mut b = TestClient::new(2);
    a.join(&mut room);
    b.join(&mut room);

    drive(&mut room, &mut [&mut a, &mut b], |_, clients| {
        clients.iter().all(|c| c.connected)
    });
    a.send_audio(10);
    b.send_audio(10);
    drive(&mut room, &mut [&mut a, &mut b], |_, clients| {
        clients.iter().all(|c| c.media_rx > 0)
    });

    // str0m paces RTP emission against real time, so frames keep
    // trickling in after the drive above returns. Drain until the
    // settle-phase media has fully landed, or stale frames would fake
    // a mute leak below.
    let drain_until = Instant::now() + Duration::from_millis(600);
    while Instant::now() < drain_until {
        step(&mut room, &mut [&mut a, &mut b]);
    }

    // Mute a's audio at the SFU. Both keep sending for a while, but b
    // must receive none of a's frames: they die at the forwarding step.
    let (a_baseline, b_baseline) = (a.media_rx, b.media_rx);
    room.set_mute(a.id, Some(true), None).unwrap();
    a.send_audio(20);
    b.send_audio(20);
    let mute_until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < mute_until {
        step(&mut room, &mut [&mut a, &mut b]);
    }
    assert_eq!(b.media_rx, b_baseline, "muted frames leaked to b");

    // b's own audio still flows to a — the mute only gates a's media.
    assert!(a.media_rx > a_baseline, "b's audio stopped flowing to a");

    // Unmute: forwarding to b resumes.
    room.set_mute(a.id, Some(false), None).unwrap();
    a.send_audio(20);
    drive(&mut room, &mut [&mut a, &mut b], |_, clients| {
        clients[1].media_rx > b_baseline
    });
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

    // Share announcement for a stranger.
    assert!(matches!(
        room.set_sharing(PeerId::new_v4(), true),
        Err(SfuError::UnknownPeer)
    ));
}

#[test]
fn screen_shares_flow_to_subscribers_and_survive_camera_mute() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let mut b = TestClient::new(2);
    a.join(&mut room);
    b.join(&mut room);
    a.send_audio(5);
    b.send_audio(5);
    drive(&mut room, &mut [&mut a, &mut b], |_, cs| {
        cs.iter().all(|c| c.connected) && cs.iter().all(|c| c.media_rx > 0)
    });

    // a shares: announce first, then add the screen m-line.
    room.set_sharing(a.id, true).unwrap();
    let screen_mid = a.share_screen(&mut room);
    let a_id = a.id;
    let b_id = b.id;
    let screen_msid = format!("{a_id}-screen");

    // The fan-out offer labels the screen with its own stream so
    // viewers can tell it apart from the camera tile. Wait until the
    // offer has actually been generated — the track entry alone is not
    // enough to guarantee negotiation has run.
    drive(&mut room, &mut [&mut a, &mut b], |r, cs| {
        r.peer_tracks(b_id).unwrap().1 == 2
            && cs[1].server_offers.iter().any(|o| o.contains(&screen_msid))
    });

    // The camera mute must not gate the screen track.
    room.set_mute(a.id, None, Some(true)).unwrap();
    let before = b.media_rx;
    a.send_screen_frames(screen_mid, 10);
    drive(&mut room, &mut [&mut a, &mut b], |_, cs| {
        cs[1].media_rx > before
    });

    // Unmuting the camera changes nothing for the screen either.
    room.set_mute(a.id, None, Some(false)).unwrap();
    let before = b.media_rx;
    a.send_screen_frames(screen_mid, 10);
    drive(&mut room, &mut [&mut a, &mut b], |_, cs| {
        cs[1].media_rx > before
    });
}

#[test]
fn stopping_a_share_winds_down_subscribers_and_reshare_reopens() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let mut b = TestClient::new(2);
    a.join(&mut room);
    b.join(&mut room);
    drive(&mut room, &mut [&mut a, &mut b], |_, cs| {
        cs.iter().all(|c| c.connected)
    });

    room.set_sharing(a.id, true).unwrap();
    let screen_mid = a.share_screen(&mut room);
    let a_id = a.id;
    let b_id = b.id;
    drive(&mut room, &mut [&mut a, &mut b], |r, _| {
        r.peer_tracks(b_id).unwrap().1 == 2
    });
    // room sweeps b's entry (stop offered, answered, dropped).
    a.set_screen_direction(&mut room, screen_mid, Direction::RecvOnly);
    let b_id = b.id;
    drive(&mut room, &mut [&mut a, &mut b], |r, _| {
        r.peer_tracks(b_id).unwrap().1 == 1
    });

    // A late joiner must not receive the dead share: the replay hands
    // over a's and b's audio but skips the stopped screen track.
    let mut c = TestClient::new(3);
    c.join(&mut room);
    let c_id = c.id;
    let screen_msid = format!("{a_id}-screen");
    drive(&mut room, &mut [&mut a, &mut b, &mut c], |r, cs| {
        r.peer_tracks(c_id).unwrap().1 == 2 && cs[2].connected
    });
    assert!(
        !c.server_offers.iter().any(|o| o.contains(&screen_msid)),
        "the stopped share was replayed to the newcomer"
    );

    // a re-shares on the same transceiver: b is re-subscribed (now
    // three outgoing entries: a's audio, c's audio, a's screen) and
    // the screen frames flow again.
    a.set_screen_direction(&mut room, screen_mid, Direction::SendRecv);
    drive(&mut room, &mut [&mut a, &mut b, &mut c], |r, _| {
        r.peer_tracks(b_id).unwrap().1 == 3
    });
    let before = b.media_rx;
    a.send_screen_frames(screen_mid, 10);
    drive(&mut room, &mut [&mut a, &mut b, &mut c], |_, cs| {
        cs[1].media_rx > before
    });
}

/// Simulcast markers: one byte per layer, first payload byte on the wire.
const HIGH: u8 = 0xC1;
const MID: u8 = 0xC2;
const LOW: u8 = 0xC3;
/// Long enough for frames written now to surface and for the SFU's 1s
/// keyframe-request throttle to age out.
const SETTLE: Duration = Duration::from_millis(1200);

#[test]
fn simulcast_routes_the_subscribers_chosen_layer() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let mut b = TestClient::new(2);
    let (a_id, b_id) = (a.id, b.id);
    a.join(&mut room);
    b.join(&mut room);
    let cam = a.publish_simulcast_camera(&mut room);

    // b gets two outgoing tracks: a's audio and a's simulcast camera.
    drive(&mut room, &mut [&mut a, &mut b], |r, cs| {
        cs.iter().all(|c| c.connected) && r.peer_tracks(b_id) == Some((1, 2))
    });

    // All three layers flow; a subscriber on the default (low) layer
    // only sees the low rid.
    a.send_simulcast_frames(cam, "high", HIGH, 5);
    a.send_simulcast_frames(cam, "mid", MID, 5);
    a.send_simulcast_frames(cam, "low", LOW, 5);
    drain(&mut room, &mut [&mut a, &mut b], SETTLE);
    assert!(b.media_rx_markers.get(&LOW).is_some_and(|n| *n > 0));
    assert!(!b.media_rx_markers.contains_key(&HIGH), "high layer leaked");
    assert!(!b.media_rx_markers.contains_key(&MID), "mid layer leaked");

    // A layer switch asks the origin for a keyframe on the newly chosen
    // rid, throttled per layer: the request lands once, an immediate
    // repeat is a no-op.
    let before = room.peer_stats(a_id).unwrap().keyframes_requested;
    room.set_layer_preference(b_id, a_id, VideoLayer::High)
        .unwrap();
    let after_switch = room.peer_stats(a_id).unwrap().keyframes_requested;
    assert_eq!(
        after_switch,
        before + 1,
        "no keyframe on the switched-to layer"
    );
    room.set_layer_preference(b_id, a_id, VideoLayer::High)
        .unwrap();
    assert_eq!(
        room.peer_stats(a_id).unwrap().keyframes_requested,
        after_switch,
        "repeat preference was not throttled"
    );

    // b now receives the high layer instead of the low one.
    let (low_before, high_before) = (
        b.media_rx_markers.get(&LOW).copied().unwrap_or(0),
        b.media_rx_markers.get(&HIGH).copied().unwrap_or(0),
    );
    a.send_simulcast_frames(cam, "high", HIGH, 5);
    a.send_simulcast_frames(cam, "low", LOW, 5);
    drain(&mut room, &mut [&mut a, &mut b], SETTLE);
    assert!(
        b.media_rx_markers
            .get(&HIGH)
            .is_some_and(|n| *n > high_before),
        "high layer did not flow after the switch"
    );
    assert_eq!(
        b.media_rx_markers.get(&LOW).copied().unwrap_or(0),
        low_before,
        "low layer still flowed after switching away"
    );

    // Switching back to low fires on the low rid's own throttle bucket.
    let before = room.peer_stats(a_id).unwrap().keyframes_requested;
    room.set_layer_preference(b_id, a_id, VideoLayer::Low)
        .unwrap();
    assert_eq!(
        room.peer_stats(a_id).unwrap().keyframes_requested,
        before + 1,
        "switching to low did not request a keyframe"
    );
    a.send_simulcast_frames(cam, "low", LOW, 5);
    drain(&mut room, &mut [&mut a, &mut b], SETTLE);
    assert!(
        b.media_rx_markers
            .get(&LOW)
            .is_some_and(|n| *n > low_before + 1),
        "low layer did not flow after switching back"
    );
}

#[test]
fn late_joiners_start_on_the_low_layer() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let mut b = TestClient::new(2);
    let (_a_id, b_id) = (a.id, b.id);
    a.join(&mut room);
    b.join(&mut room);
    let cam = a.publish_simulcast_camera(&mut room);
    drive(&mut room, &mut [&mut a, &mut b], |r, cs| {
        cs.iter().all(|c| c.connected) && r.peer_tracks(b_id) == Some((1, 2))
    });

    // c joins after the camera is already flowing. It replays every
    // existing track: a's audio and camera plus b's audio.
    let mut c = TestClient::new(3);
    c.join(&mut room);
    let c_id = c.id;
    drive(&mut room, &mut [&mut a, &mut b, &mut c], |r, cs| {
        cs[2].connected && r.peer_tracks(c_id) == Some((1, 3))
    });

    a.send_simulcast_frames(cam, "high", HIGH, 5);
    a.send_simulcast_frames(cam, "mid", MID, 5);
    a.send_simulcast_frames(cam, "low", LOW, 5);
    drain(&mut room, &mut [&mut a, &mut b, &mut c], SETTLE);
    assert!(c.media_rx_markers.get(&LOW).is_some_and(|n| *n > 0));
    assert!(
        !c.media_rx_markers.contains_key(&HIGH),
        "late joiner received the high layer"
    );
    assert!(
        !c.media_rx_markers.contains_key(&MID),
        "late joiner received the mid layer"
    );
}

#[test]
fn screen_shares_ignore_layer_preferences() {
    let mut room = Room::new(MEDIA_ADDR);
    let mut a = TestClient::new(1);
    let mut b = TestClient::new(2);
    let (a_id, b_id) = (a.id, b.id);
    a.join(&mut room);
    b.join(&mut room);
    let cam = a.publish_simulcast_camera(&mut room);
    // Let the camera m-line surface on the SFU before arming the share:
    // MediaAdded fires on poll, and the screen_pending heuristic tags the
    // *next* remote video m-line — without this drive the camera would be
    // misclassified as the screen (in production the camera always lands
    // in the initial offer, long before any Sharing announcement).
    drive(&mut room, &mut [&mut a, &mut b], |r, cs| {
        cs.iter().all(|c| c.connected) && r.peer_tracks(a_id).is_some_and(|(i, _)| i == 2)
    });
    room.set_sharing(a_id, true).unwrap();
    let screen = a.share_screen(&mut room);

    // b's outgoing tracks: a's audio, camera and screen.
    drive(&mut room, &mut [&mut a, &mut b], |r, cs| {
        cs.iter().all(|c| c.connected) && r.peer_tracks(b_id) == Some((1, 3))
    });

    // Screen frames flow regardless of the camera's layer preference.
    a.send_simulcast_frames(cam, "high", HIGH, 5);
    a.send_simulcast_frames(cam, "low", LOW, 5);
    a.send_screen_frames(screen, 5);
    drain(&mut room, &mut [&mut a, &mut b], SETTLE);
    let (screen_before, low_before) = (
        b.media_rx_markers.get(&0x22).copied().unwrap_or(0),
        b.media_rx_markers.get(&LOW).copied().unwrap_or(0),
    );
    assert!(screen_before > 0, "screen frames did not flow");

    // Focusing (high layer) must not touch the single-layer screen.
    room.set_layer_preference(b_id, a_id, VideoLayer::High)
        .unwrap();
    a.send_simulcast_frames(cam, "high", HIGH, 5);
    a.send_simulcast_frames(cam, "low", LOW, 5);
    a.send_screen_frames(screen, 5);
    drain(&mut room, &mut [&mut a, &mut b], SETTLE);
    assert!(
        b.media_rx_markers.get(&HIGH).is_some_and(|n| *n > 0),
        "high layer did not flow after the preference"
    );
    assert!(
        b.media_rx_markers
            .get(&0x22)
            .is_some_and(|n| *n > screen_before),
        "screen frames were affected by the layer preference"
    );
    assert_eq!(
        b.media_rx_markers.get(&LOW).copied().unwrap_or(0),
        low_before,
        "low layer still flowed after switching away"
    );
}
