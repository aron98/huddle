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
    RtcPeerConnection, RtcPeerConnectionIceEvent, RtcRtpSender, RtcRtpTransceiverDirection,
    RtcRtpTransceiverInit, RtcSdpType, RtcSessionDescription, RtcSessionDescriptionInit,
    RtcTrackEvent, WebSocket,
};

use huddle_protocol::{ClientMessage, IceServer, ServerMessage};

use super::peer::{LocalCandidate, PeerConnection, PeerError};
use super::transport::{SignalingError, SignalingStream};

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
    /// Track + carrying stream staged by the UI, awaiting attachment.
    staged: Option<(MediaStreamTrack, MediaStream)>,
    /// The sender created by the last `add_screen_track`.
    sender: Option<RtcRtpSender>,
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
}

impl BrowserPeerConnection {
    pub fn new(ice_servers: &[IceServer]) -> Result<Self, PeerError> {
        let configuration = RtcConfiguration::new();
        let servers = js_sys::Array::new();
        for server in ice_servers {
            servers.push(&ice_server_to_js(server)?);
        }
        configuration.set_ice_servers(&servers);

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
        let remote_streams = Rc::new(RefCell::new(Vec::<(String, MediaStream)>::new()));
        let on_remote_stream: RemoteStreamCallback = Rc::new(RefCell::new(None));
        let streams_sink = Rc::clone(&remote_streams);
        let callback_sink = Rc::clone(&on_remote_stream);
        let on_track = Closure::<dyn FnMut(RtcTrackEvent)>::new(move |event: RtcTrackEvent| {
            let streams = event.streams();
            for i in 0..streams.length() {
                let Ok(stream) = streams.get(i).dyn_into::<MediaStream>() else {
                    continue;
                };
                let id = stream.id();
                let mut seen = streams_sink.borrow_mut();
                if seen.iter().any(|(existing, _)| existing == &id) {
                    continue;
                }
                seen.push((id.clone(), stream.clone()));
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

    /// Add every track of `stream` to the peer connection (as sender),
    /// so the next offer announces our camera and microphone.
    pub fn attach_stream(&self, stream: &MediaStream) {
        let tracks = stream.get_tracks();
        for i in 0..tracks.length() {
            if let Ok(track) = tracks.get(i).dyn_into::<MediaStreamTrack>() {
                self.pc.add_track_0(&track, stream);
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
        Ok(())
    }

    async fn remove_screen_track(&mut self) -> Result<(), PeerError> {
        let mut screen = self.screen.borrow_mut();
        // Clear any staged-but-never-attached track too, so a fresh share
        // stages on a clean slate.
        screen.staged = None;
        if let Some(sender) = screen.sender.take() {
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
