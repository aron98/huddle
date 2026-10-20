//! Browser implementations of the [`SignalingStream`] and
//! [`PeerConnection`] traits, over `WebSocket` and `RTCPeerConnection`.
//!
//! Everything here is platform glue; the sequencing rules live in
//! [`RtcClient`](super::RtcClient), shared with the native tests.

use std::cell::RefCell;
use std::rc::Rc;

use futures::StreamExt;
use futures::channel::mpsc;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MediaStream, MediaStreamTrack, RtcConfiguration, RtcIceCandidate, RtcIceCandidateInit,
    RtcIceTransportPolicy, RtcPeerConnection, RtcPeerConnectionIceEvent, RtcRtpEncodingParameters,
    RtcRtpSender, RtcRtpTransceiverDirection, RtcRtpTransceiverInit, RtcSdpType,
    RtcSessionDescription, RtcSessionDescriptionInit, RtcTrackEvent, WebSocket,
};

use huddle_protocol::{ClientMessage, IceServer, ServerMessage};

use super::peer::{LocalCandidate, PeerConnection, PeerError};
use super::transport::{SignalingError, SignalingStream};

/// Snapshot the track ids a stream currently carries, as plain `String`s —
/// the ids are unique per delivery and `MediaStream::id()` itself is
/// unstable across reads, so capture them once, at event time.
fn track_ids(stream: &MediaStream) -> Vec<String> {
    let tracks = stream.get_tracks();
    let mut ids = Vec::with_capacity(tracks.length() as usize);
    for i in 0..tracks.length() {
        if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
            ids.push(track.id());
        }
    }
    ids
}

/// A [`SignalingStream`] over a browser `WebSocket`.
///
/// Event handlers push into an unbounded channel that `next_message` drains,
/// bridging the callback-based DOM API to our async trait.
pub struct BrowserSignalingStream {
    socket: WebSocket,
    receiver: mpsc::UnboundedReceiver<SocketEvent>,
    // Keep the JS closures alive as long as the socket is.
    _on_open: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_error: Closure<dyn FnMut()>,
    _on_close: Closure<dyn FnMut()>,
}

/// What the socket event handlers observed.
enum SocketEvent {
    Open,
    Message(ServerMessage),
    Failed(String),
    Closed,
}

impl BrowserSignalingStream {
    /// Open a WebSocket and wait for it to connect.
    ///
    /// `send` throws `InvalidStateError` while the socket is still in the
    /// CONNECTING state, so callers must not send before the handshake
    /// finished — hence `async` and the explicit wait for `open`.
    pub async fn connect(url: &str) -> Result<Self, SignalingError> {
        let socket = WebSocket::new(url)
            .map_err(|e| SignalingError::Socket(format!("cannot open {url}: {e:?}")))?;

        let (sender, receiver) = mpsc::unbounded();

        let on_open = {
            let sender = sender.clone();
            Closure::<dyn FnMut()>::new(move || {
                let _ = sender.unbounded_send(SocketEvent::Open);
            })
        };
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));

        let on_message = {
            let sender = sender.clone();
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                let Some(text) = event.data().as_string() else {
                    let _ = sender.unbounded_send(SocketEvent::Failed(
                        "binary signaling frames are not supported".to_owned(),
                    ));
                    return;
                };
                match serde_json::from_str(&text) {
                    Ok(message) => {
                        let _ = sender.unbounded_send(SocketEvent::Message(message));
                    }
                    Err(e) => {
                        let _ = sender.unbounded_send(SocketEvent::Failed(format!(
                            "malformed server message: {e}"
                        )));
                    }
                }
            })
        };
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let on_error = {
            let sender = sender.clone();
            Closure::<dyn FnMut()>::new(move || {
                let _ = sender.unbounded_send(SocketEvent::Failed(
                    "the signaling socket errored".to_owned(),
                ));
            })
        };
        socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));

        let on_close = {
            let sender = sender.clone();
            Closure::<dyn FnMut()>::new(move || {
                let _ = sender.unbounded_send(SocketEvent::Closed);
            })
        };
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        let mut stream = Self {
            socket,
            receiver,
            _on_open: on_open,
            _on_message: on_message,
            _on_error: on_error,
            _on_close: on_close,
        };

        // The handshake is asynchronous: hold the stream back until the
        // socket reports `open` (or fails trying).
        loop {
            match stream.receiver.next().await {
                Some(SocketEvent::Open) => return Ok(stream),
                // Messages cannot arrive before `open`; be forgiving.
                Some(SocketEvent::Message(_)) => continue,
                Some(SocketEvent::Failed(e)) => {
                    return Err(SignalingError::Socket(format!(
                        "signaling socket failed: {e}"
                    )));
                }
                Some(SocketEvent::Closed) => {
                    return Err(SignalingError::Socket(
                        "signaling socket closed before opening".to_owned(),
                    ));
                }
                None => {
                    return Err(SignalingError::Socket(
                        "signaling socket event stream gone".to_owned(),
                    ));
                }
            }
        }
    }
}

impl Drop for BrowserSignalingStream {
    fn drop(&mut self) {
        // Detach the handlers *before* the closures drop: a late DOM event
        // firing into a dropped closure panics the whole wasm module
        // ("closure invoked recursively or after being dropped").
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onerror(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

impl SignalingStream for BrowserSignalingStream {
    async fn send(&mut self, message: ClientMessage) -> Result<(), SignalingError> {
        let text = serde_json::to_string(&message)
            .map_err(|e| SignalingError::Socket(format!("cannot encode message: {e}")))?;
        self.socket
            .send_with_str(&text)
            .map_err(|e| SignalingError::Socket(format!("signaling send failed: {e:?}")))
    }

    async fn next_message(&mut self) -> Option<Result<ServerMessage, SignalingError>> {
        loop {
            match self.receiver.next().await? {
                SocketEvent::Open => continue,
                SocketEvent::Message(message) => return Some(Ok(message)),
                SocketEvent::Failed(reason) => {
                    return Some(Err(SignalingError::Socket(reason)));
                }
                SocketEvent::Closed => return None,
            }
        }
    }
}

/// A [`PeerConnection`] over the browser's `RTCPeerConnection`.
pub struct BrowserPeerConnection {
    pc: RtcPeerConnection,
    /// Whether this connection was built with the `?turn=relay` knob;
    /// `setConfiguration` re-asserts it (a fresh RTCConfiguration would
    /// otherwise reset the policy to "all").
    relay: bool,
    local_candidates: Rc<RefCell<Vec<LocalCandidate>>>,
    on_remote_stream: RemoteStreamCallback,
    /// Screen-share state shared with the [`ScreenShareHandle`] the UI
    /// holds; see [`BrowserPeerConnection::screen_share_handle`].
    screen: SharedScreenState,
    _on_ice_candidate: Closure<dyn FnMut(RtcPeerConnectionIceEvent)>,
    _on_track: Closure<dyn FnMut(RtcTrackEvent)>,
}

/// A Rust callback invoked for every new remote [`MediaStream`], with the
/// stream id captured at `ontrack` time (see the comment in `new` for why
/// the id must be captured then rather than re-read later).
type RemoteStreamCallback = Rc<RefCell<Option<Box<dyn FnMut(&str, MediaStream)>>>>;

/// Screen-share state, shared between the session task (which owns the
/// [`BrowserPeerConnection`]) and the UI (which holds a
/// [`ScreenShareHandle`]): the UI stages the `getDisplayMedia` track it
/// acquired, the task attaches it during `add_screen_track`.
#[derive(Default)]
struct ScreenState {
    /// Video track + carrying stream staged by the UI, awaiting attachment.
    staged: Option<(MediaStreamTrack, MediaStream)>,
    /// Computer/tab audio staged alongside the video, when the capture
    /// has one.
    staged_audio: Option<(MediaStreamTrack, MediaStream)>,
    /// The sender created by the last `add_screen_track`.
    sender: Option<RtcRtpSender>,
    /// The sender for the share's audio track, when one was attached.
    audio_sender: Option<RtcRtpSender>,
}

type SharedScreenState = Rc<RefCell<ScreenState>>;

/// Handle for the UI to stage a screen track for the session task. The
/// session task is the only side that attaches/removes it — via
/// [`PeerConnection::add_screen_track`] /
/// [`PeerConnection::remove_screen_track`] — so the negotiation sequence
/// stays inside `RtcClient`.
pub struct ScreenShareHandle(SharedScreenState);

impl ScreenShareHandle {
    /// Stage `track` (carried by `stream`) so the session task's next
    /// `add_screen_track` attaches it to the peer connection.
    pub fn stage(&self, track: MediaStreamTrack, stream: MediaStream) {
        self.0.borrow_mut().staged = Some((track, stream));
    }

    /// Stage the share's computer/tab audio track: `add_screen_track`
    /// attaches it on a second sendonly transceiver so it travels on
    /// its own m-line, exactly like the screen video.
    pub fn stage_audio(&self, track: MediaStreamTrack, stream: MediaStream) {
        self.0.borrow_mut().staged_audio = Some((track, stream));
    }
}

impl BrowserPeerConnection {
    pub fn new(ice_servers: &[IceServer]) -> Result<Self, PeerError> {
        Self::build(ice_servers, false)
    }

    /// Test knob (`?turn=relay`): route every candidate through the
    /// configured TURN relay by setting `iceTransportPolicy: "relay"`.
    ///
    /// The TURN servers themselves are not known yet — they arrive in
    /// the `Welcome` and are applied via
    /// [`PeerConnection::update_ice_servers`] before any offer is
    /// created.
    pub fn new_relay(ice_servers: &[IceServer]) -> Result<Self, PeerError> {
        Self::build(ice_servers, true)
    }

    fn build(ice_servers: &[IceServer], relay: bool) -> Result<Self, PeerError> {
        let configuration = RtcConfiguration::new();
        let servers = js_sys::Array::new();
        for server in ice_servers {
            servers.push(&ice_server_to_js(server)?);
        }
        configuration.set_ice_servers(&servers);
        if relay {
            configuration.set_ice_transport_policy(RtcIceTransportPolicy::Relay);
        }

        let pc = RtcPeerConnection::new_with_configuration(&configuration).map_err(js_error)?;

        let local_candidates = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&local_candidates);
        let on_ice_candidate = Closure::<dyn FnMut(RtcPeerConnectionIceEvent)>::new(
            move |event: RtcPeerConnectionIceEvent| {
                // `candidate: None` announces end-of-gathering; skip it.
                if let Some(candidate) = event.candidate() {
                    sink.borrow_mut().push(LocalCandidate {
                        candidate: candidate.candidate(),
                        sdp_mid: candidate.sdp_mid(),
                        sdp_m_line_index: candidate.sdp_m_line_index(),
                    });
                }
            },
        );
        pc.set_onicecandidate(Some(on_ice_candidate.as_ref().unchecked_ref()));

        // The server labels each outgoing m-line with the originating
        // participant's id (`a=msid`), so `event.streams[0].id` is the
        // origin participant's uuid. `ontrack` fires once per track, so
        // deduplicate by stream id before notifying.
        //
        // In Chrome the `MediaStream` object handed to `ontrack` does not
        // report a stable `.id()`: the same stored object can read as one
        // id during the event and a different one microseconds later
        // (observed with a three-party call; presumably the id is
        // normalised to the SDP msid asynchronously). The id read *at
        // event time* is the correct msid, so capture it into a `String`
        // and deduplicate — and carry the id onward — instead of ever
        // re-reading `.id()` from a stored stream.
        //
        // Deduplication still has to let a *re-delivered* stream through:
        // the server labels every reshare m-line with the same
        // `{origin}-screen` msid, so `ontrack` fires again with the same
        // stream id after a stop+reshare cycle. Remembering ids alone
        // would swallow the new stream and no viewer would ever see a
        // reshared screen. Track ids are unique per delivery, so snapshot
        // the track ids each stream carried when it was delivered and
        // re-deliver whenever a fire carries a track we have not seen in
        // that stream. `setRemoteDescription` is atomic (all tracks of a
        // stream attach before any `ontrack` fires), so the first fire of
        // a camera stream already sees the other track's id in the
        // snapshot and the second fire is skipped.
        let remote_streams = Rc::new(RefCell::new(Vec::<(String, Vec<String>)>::new()));
        let on_remote_stream: RemoteStreamCallback = Rc::new(RefCell::new(None));
        let streams_sink = Rc::clone(&remote_streams);
        let callback_sink = Rc::clone(&on_remote_stream);
        let on_track = Closure::<dyn FnMut(RtcTrackEvent)>::new(move |event: RtcTrackEvent| {
            let track_id = event.track().id();
            let streams = event.streams();
            for i in 0..streams.length() {
                let Ok(stream) = streams.get(i).dyn_into::<MediaStream>() else {
                    continue;
                };
                let id = stream.id();
                let mut seen = streams_sink.borrow_mut();
                let entry = seen.iter_mut().find(|(existing, _)| existing == &id);
                match entry {
                    // A track this stream already delivered: the other
                    // track of the same atomic `setRemoteDescription`.
                    Some((_, tracks)) if tracks.contains(&track_id) => continue,
                    // A new track under a known stream id: a re-delivery
                    // (reshare) — refresh the snapshot and deliver again.
                    Some(entry) => *entry = (id.clone(), track_ids(&stream)),
                    // First time this stream id is seen.
                    None => {
                        seen.push((id.clone(), track_ids(&stream)));
                    }
                }
                drop(seen);
                let mut callback = callback_sink.borrow_mut();
                if let Some(notify) = callback.as_mut() {
                    notify(&id, stream);
                }
            }
        });
        pc.set_ontrack(Some(on_track.as_ref().unchecked_ref()));

        Ok(Self {
            pc,
            relay,
            local_candidates,
            on_remote_stream,
            screen: Rc::new(RefCell::new(ScreenState::default())),
            _on_ice_candidate: on_ice_candidate,
            _on_track: on_track,
        })
    }

    /// Register a callback fired once for every NEW remote stream, with the
    /// stream id captured at `ontrack` time (stable msid) and the stream.
    pub fn set_on_remote_stream(&self, callback: impl FnMut(&str, MediaStream) + 'static) {
        *self.on_remote_stream.borrow_mut() = Some(Box::new(callback));
    }

    /// A handle the UI uses to stage a `getDisplayMedia` track; the session
    /// task attaches it via [`PeerConnection::add_screen_track`].
    pub fn screen_share_handle(&self) -> ScreenShareHandle {
        ScreenShareHandle(Rc::clone(&self.screen))
    }

    /// The underlying JS `RTCPeerConnection`, for debug/test inspection.
    pub fn raw(&self) -> RtcPeerConnection {
        self.pc.clone()
    }

    /// Publish the local microphone and camera. Camera video goes out as
    /// three simulcast layers (high quality first), so the SFU can switch
    /// each subscriber between layers; the microphone stays single-stream.
    pub async fn attach_stream(&self, stream: &MediaStream) {
        let tracks = stream.get_tracks();
        for i in 0..tracks.length() {
            if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
                if track.kind() == "video" {
                    self.publish_camera(&track, stream).await;
                } else {
                    self.pc.add_track_0(&track, stream);
                }
            }
        }
    }

    /// Publish the camera as two simulcast layers: full resolution (the
    /// high layer) and a quarter-scale thumbnail (the low layer), rids
    /// ordered high → low to mirror the SFU's layer convention.
    ///
    /// Rids are fixed at transceiver creation: `setParameters` treats
    /// `rid` as read-only (Chrome rejects any attempt with
    /// `InvalidModificationError`), so `addTransceiver` with
    /// `sendEncodings` is the only way to arm them. The per-layer bitrate
    /// budgets then go through `RtpSender::setParameters`, which is the
    /// mutable knob. Screen shares never go through here — they stay
    /// single-layer.
    ///
    /// Two layers, tightly budgeted: Chrome allocates simulcast
    /// bandwidth top-down and reliably activates only the first two
    /// encodings — a third layer sits at zero bytes forever (observed
    /// with 500/200/100 kbps caps: high and mid consumed the whole
    /// estimate, low never encoded a frame), which starves every
    /// subscriber the SFU defaults onto the low layer. With two layers
    /// the low layer is always among the active ones, so late joiners
    /// and unfocused tiles keep a live picture.
    ///
    /// A `setParameters` rejection is non-fatal: the layers keep their
    /// creation-time scale factors and the SFU simply forwards whatever
    /// arrives.
    async fn publish_camera(&self, track: &MediaStreamTrack, stream: &MediaStream) {
        let encodings = js_sys::Array::new();
        for (rid, scale) in [("high", 1.0_f32), ("low", 4.0)] {
            let encoding = RtcRtpEncodingParameters::new();
            encoding.set_rid(rid);
            encoding.set_active(true);
            encoding.set_scale_resolution_down_by(scale);
            encodings.push(&encoding);
        }
        let init = RtcRtpTransceiverInit::new();
        init.set_direction(RtcRtpTransceiverDirection::Sendrecv);
        let streams = js_sys::Array::new();
        streams.push(stream);
        init.set_streams(&streams);
        init.set_send_encodings(&encodings);
        let transceiver = self
            .pc
            .add_transceiver_with_media_stream_track_and_init(track, &init);
        let sender = transceiver.sender();

        // Tune the layer budgets via setParameters: 500 kbps for the full
        // layer, 100 for the quarter-scale thumbnail. The tight high-layer
        // cap leaves room for the low layer inside Chrome's top-down
        // allocation (see the doc comment above).
        //
        // The encodings come back from `getParameters()` as plain JS
        // dictionaries, not `RTCRtpEncodingParameters` interface
        // instances, so `dyn_into` cannot cast them — the bitrates are
        // set with `Reflect::set` on the raw values instead.
        let params = sender.get_parameters();
        if let Some(existing) = params.get_encodings() {
            for (i, max_bitrate) in [500_000_u32, 100_000].into_iter().enumerate() {
                let encoding = existing.get(i as u32);
                let name = JsValue::from("maxBitrate");
                if js_sys::Reflect::set(&encoding, &name, &JsValue::from(max_bitrate)).is_err() {
                    leptos::logging::warn!("simulcast encoding {i} has no maxBitrate field");
                }
            }
            let promise = sender.set_parameters_with_parameters(&params);
            if let Err(e) = JsFuture::from(promise).await {
                leptos::logging::warn!("simulcast setParameters rejected: {e:?}");
            }
        }
    }

    async fn describe_local(&self, kind: RtcSdpType, sdp: &str) -> Result<(), PeerError> {
        let description = local_or_remote_description(kind, sdp);
        JsFuture::from(self.pc.set_local_description(&description))
            .await
            .map_err(js_error)
            .map(drop)
    }

    async fn apply_remote(&self, kind: RtcSdpType, sdp: &str) -> Result<(), PeerError> {
        let description = local_or_remote_description(kind, sdp);
        JsFuture::from(self.pc.set_remote_description(&description))
            .await
            .map_err(js_error)
            .map(drop)
    }
}

impl Drop for BrowserPeerConnection {
    fn drop(&mut self) {
        // Detach the handlers *before* the closures drop: ICE candidate
        // gathering and track events keep firing on their own schedule, and
        // a late event calling a dropped closure panics the wasm module
        // ("closure invoked recursively or after being dropped").
        self.pc.set_onicecandidate(None);
        self.pc.set_ontrack(None);
        self.pc.close();
    }
}

fn local_or_remote_description(kind: RtcSdpType, sdp: &str) -> RtcSessionDescriptionInit {
    let description = RtcSessionDescriptionInit::new(kind);
    description.set_sdp(sdp);
    description
}

impl PeerConnection for BrowserPeerConnection {
    async fn create_offer(&mut self) -> Result<String, PeerError> {
        let offer = JsFuture::from(self.pc.create_offer())
            .await
            .map_err(js_error)?;
        let sdp = RtcSessionDescription::from(offer).sdp();
        self.describe_local(RtcSdpType::Offer, &sdp).await?;
        Ok(sdp)
    }

    async fn apply_answer(&mut self, sdp: String) -> Result<(), PeerError> {
        self.apply_remote(RtcSdpType::Answer, &sdp).await
    }

    async fn apply_remote_offer(&mut self, sdp: String) -> Result<String, PeerError> {
        self.apply_remote(RtcSdpType::Offer, &sdp).await?;
        let answer = JsFuture::from(self.pc.create_answer())
            .await
            .map_err(js_error)?;
        let answer_sdp = RtcSessionDescription::from(answer).sdp();
        self.describe_local(RtcSdpType::Answer, &answer_sdp).await?;
        Ok(answer_sdp)
    }

    async fn add_remote_candidate(&mut self, candidate: LocalCandidate) -> Result<(), PeerError> {
        let init = RtcIceCandidateInit::new(&candidate.candidate);
        init.set_sdp_mid(candidate.sdp_mid.as_deref());
        init.set_sdp_m_line_index(candidate.sdp_m_line_index);
        let remote = RtcIceCandidate::new(&init).map_err(js_error)?;
        JsFuture::from(
            self.pc
                .add_ice_candidate_with_opt_rtc_ice_candidate(Some(&remote)),
        )
        .await
        .map_err(js_error)
        .map(drop)
    }

    fn take_local_candidates(&mut self) -> Vec<LocalCandidate> {
        std::mem::take(&mut *self.local_candidates.borrow_mut())
    }

    fn update_ice_servers(&mut self, ice_servers: &[IceServer]) -> Result<(), PeerError> {
        if ice_servers.is_empty() {
            return Ok(());
        }
        let configuration = RtcConfiguration::new();
        let servers = js_sys::Array::new();
        for server in ice_servers {
            servers.push(&ice_server_to_js(server)?);
        }
        configuration.set_ice_servers(&servers);
        // A freshly-built RTCConfiguration carries the default
        // `iceTransportPolicy: "all"`, and Chrome's setConfiguration
        // treats every present field as authoritative — so it would
        // silently reset the relay knob. Re-assert our own policy.
        if self.relay {
            configuration.set_ice_transport_policy(RtcIceTransportPolicy::Relay);
        }
        self.pc
            .set_configuration_with_configuration(&configuration)
            .map_err(js_error)
    }

    async fn add_screen_track(&mut self) -> Result<(), PeerError> {
        let (track, stream) = self
            .screen
            .borrow_mut()
            .staged
            .take()
            .ok_or_else(|| PeerError::Failed("no screen track staged".to_owned()))?;
        // `addTrack` would not create a fresh m-line here: it happily
        // reuses one of the many recvonly transceivers the server's
        // forwarded tracks created (flipping it to sendrecv and silently
        // hijacking that tile's m-line). `addTransceiver` always creates
        // a new m-line, which the SFU sees as a new (screen) track.
        let init = RtcRtpTransceiverInit::new();
        init.set_direction(RtcRtpTransceiverDirection::Sendonly);
        let streams = js_sys::Array::new();
        streams.push(&stream);
        init.set_streams(&streams);
        let transceiver = self
            .pc
            .add_transceiver_with_media_stream_track_and_init(&track, &init);
        self.screen.borrow_mut().sender = Some(transceiver.sender());

        // The share's computer/tab audio, when the user kept one in the
        // picker: a second sendonly transceiver — never `addTrack`, for
        // the same m-line-hijacking reason as above — so it gets its
        // own m-line and the SFU forwards it like any other audio mid.
        // Both transceivers reference the same capture stream, so the
        // two m-lines carry the same msid and the SFU re-labels them
        // with its own `{origin}-screen` id on the way out.
        // NOTE: hoist the `take()` out of the `if let` scrutinee — the
        // scrutinee's temporary `borrow_mut()` would otherwise stay
        // alive for the whole block and the second `borrow_mut()` below
        // would panic ("RefCell already borrowed") in the browser.
        let staged_audio = self.screen.borrow_mut().staged_audio.take();
        if let Some((audio, audio_stream)) = staged_audio {
            let init = RtcRtpTransceiverInit::new();
            init.set_direction(RtcRtpTransceiverDirection::Sendonly);
            let streams = js_sys::Array::new();
            streams.push(&audio_stream);
            init.set_streams(&streams);
            let transceiver = self
                .pc
                .add_transceiver_with_media_stream_track_and_init(&audio, &init);
            self.screen.borrow_mut().audio_sender = Some(transceiver.sender());
        }
        Ok(())
    }

    async fn remove_screen_track(&mut self) -> Result<(), PeerError> {
        let mut screen = self.screen.borrow_mut();
        // Clear any staged-but-never-attached tracks too, so a fresh
        // share stages on a clean slate.
        screen.staged = None;
        screen.staged_audio = None;
        if let Some(sender) = screen.sender.take() {
            self.pc.remove_track(&sender);
        }
        if let Some(sender) = screen.audio_sender.take() {
            self.pc.remove_track(&sender);
        }
        Ok(())
    }
}

/// Translate one ICE/TURN server description into a JS `RTCIceServer`.
fn ice_server_to_js(server: &IceServer) -> Result<JsValue, PeerError> {
    let object = js_sys::Object::new();
    let urls = js_sys::Array::from_iter(server.urls.iter().map(|u| JsValue::from_str(u.as_str())));
    js_sys::Reflect::set(&object, &JsValue::from_str("urls"), &urls)
        .map_err(|e| PeerError::Failed(format!("cannot set urls: {e:?}")))?;
    if let Some(username) = &server.username {
        js_sys::Reflect::set(
            &object,
            &JsValue::from_str("username"),
            &JsValue::from_str(username),
        )
        .map_err(|e| PeerError::Failed(format!("cannot set username: {e:?}")))?;
    }
    if let Some(credential) = &server.credential {
        js_sys::Reflect::set(
            &object,
            &JsValue::from_str("credential"),
            &JsValue::from_str(credential),
        )
        .map_err(|e| PeerError::Failed(format!("cannot set credential: {e:?}")))?;
    }
    Ok(object.into())
}

fn js_error(value: JsValue) -> PeerError {
    PeerError::Failed(format!("{value:?}"))
}
