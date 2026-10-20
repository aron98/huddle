//! Camera/microphone acquisition and device enumeration.

use crate::audio_settings::{self, AudioSettings};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MediaDeviceKind, MediaDevices, MediaStream, MediaStreamConstraints, MediaStreamTrack, Navigator,
};

/// An input device the user can pick in the prejoin page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub label: String,
    pub kind: DeviceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Camera,
    Microphone,
}

/// Acquire the camera and microphone. `camera_id`/`mic_id` select specific
/// devices (`None` = browser default); used by the prejoin pickers.
/// `audio` carries the explicit echo-cancellation / noise-suppression
/// constraints (issue #34) — they apply on this and every
/// re-acquisition alike.
pub async fn request_media(
    camera_id: Option<&str>,
    mic_id: Option<&str>,
    audio: AudioSettings,
) -> Result<MediaStream, String> {
    let devices = media_devices()?;

    let constraints = MediaStreamConstraints::new();
    constraints.set_audio(&audio_settings::mic_constraint(mic_id, audio));
    match camera_id {
        Some(id) => constraints.set_video(&exact_device(id)),
        None => constraints.set_video_bool(true),
    }

    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|e| format!("cannot call getUserMedia: {e:?}"))?;
    let stream = JsFuture::from(promise)
        .await
        .map_err(|e| format!("getUserMedia denied or failed: {e:?}"))?;
    stream
        .dyn_into::<MediaStream>()
        .map_err(|e| format!("getUserMedia returned {e:?}"))
}

/// Acquire only the microphone, with the same explicit audio
/// constraints [`request_media`] applies — used by the in-call audio
/// settings swap, where the camera track must not be disturbed.
pub async fn request_mic(
    mic_id: Option<&str>,
    audio: AudioSettings,
) -> Result<MediaStreamTrack, String> {
    let devices = media_devices()?;

    let constraints = MediaStreamConstraints::new();
    constraints.set_audio(&audio_settings::mic_constraint(mic_id, audio));

    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|e| format!("cannot call getUserMedia: {e:?}"))?;
    let stream = JsFuture::from(promise)
        .await
        .map_err(|e| format!("getUserMedia denied or failed: {e:?}"))?;
    let stream = stream
        .dyn_into::<MediaStream>()
        .map_err(|e| format!("getUserMedia returned {e:?}"))?;
    tracks_of(&stream)
        .into_iter()
        .find(|t| t.kind() == "audio")
        .ok_or_else(|| "microphone stream has no audio track".to_owned())
}

/// List cameras and microphones. Labels are only populated once the page
/// holds capture permission, so call [`request_media`] first.
pub async fn list_devices() -> Result<Vec<Device>, String> {
    let devices = media_devices()?;
    let promise = devices
        .enumerate_devices()
        .map_err(|e| format!("cannot enumerate devices: {e:?}"))?;
    let found = JsFuture::from(promise)
        .await
        .map_err(|e| format!("device enumeration failed: {e:?}"))?;

    let mut out = Vec::new();
    let found = js_sys::Array::from(&found);
    for i in 0..found.length() {
        let Ok(info) = found.get(i).dyn_into::<web_sys::MediaDeviceInfo>() else {
            continue;
        };
        let kind = match info.kind() {
            MediaDeviceKind::Videoinput => DeviceKind::Camera,
            MediaDeviceKind::Audioinput => DeviceKind::Microphone,
            // The enum is `#[non_exhaustive]`; audio outputs are not useful
            // here either.
            _ => continue,
        };
        let label = if info.label().is_empty() {
            match kind {
                DeviceKind::Camera => "Camera",
                DeviceKind::Microphone => "Microphone",
            }
            .to_owned()
        } else {
            info.label()
        };
        out.push(Device {
            id: info.device_id(),
            label,
            kind,
        });
    }
    Ok(out)
}

/// The media tracks of `stream`, for mute toggles and teardown.
pub fn tracks_of(stream: &MediaStream) -> Vec<MediaStreamTrack> {
    let tracks = stream.get_tracks();
    (0..tracks.length())
        .filter_map(|i| tracks.get(i).dyn_into::<MediaStreamTrack>().ok())
        .collect()
}

/// Enable or disable every track of one kind (`"audio"` or `"video"`) in
/// the stream, leaving the other kind untouched. Both the prejoin and
/// in-call toggles go through here so the two pages cannot drift into
/// muting the whole stream again (issue #7).
pub fn set_track_kind_enabled(stream: &MediaStream, kind: &str, enabled: bool) {
    for track in tracks_of(stream) {
        if track.kind() == kind {
            track.set_enabled(enabled);
        }
    }
}

/// Whether every track of `kind` in the stream is enabled. A stream with
/// no such track reads as enabled — there is nothing muted about it — so
/// the call view can seed its toggle buttons straight from the local
/// stream and have a prejoin mute survive the join.
pub fn track_kind_enabled(stream: &MediaStream, kind: &str) -> bool {
    tracks_of(stream)
        .into_iter()
        .filter(|track| track.kind() == kind)
        .all(|track| track.enabled())
}

/// Root-mean-square level above which the local audio counts as speech.
const SPEAKING_RMS: f32 = 0.03;
/// How often the level is sampled.
const SPEAKING_POLL_MS: i32 = 200;
/// Quiet samples before speech is considered over (≈1s hangover).
const SPEAKING_HANGOVER_TICKS: u32 = 5;
/// Size of the analysis window; small is plenty for an RMS gate.
const SPEAKING_WINDOW: usize = 256;

/// Keeps the Web Audio graph and sampling timer alive. Dropping the guard
/// stops the speaking detection.
pub struct SpeakingGuard {
    // Field order matters: the closure must drop before the nodes it
    // borrows. Rust drops fields in declaration order, so `_nodes` last
    // would still be fine, but the explicit order documents the intent.
    _interval_handle: i32,
    _callback: wasm_bindgen::closure::Closure<dyn FnMut()>,
    _nodes: (
        web_sys::AudioContext,
        web_sys::MediaStreamAudioSourceNode,
        web_sys::AnalyserNode,
        web_sys::GainNode,
    ),
}

impl Drop for SpeakingGuard {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            window.clear_interval_with_handle(self._interval_handle);
        }
    }
}

/// Watch `stream`'s audio level and call `on_change` whenever the
/// speaking/not-speaking verdict flips. A muted track reads as silence.
///
/// The graph is source → analyser → zero-gain sink → destination: the sink
/// keeps the browser pulling samples through the analyser without playing
/// the local mic back to the user.
pub async fn watch_speaking(
    stream: &MediaStream,
    mut on_change: impl FnMut(bool) + 'static,
) -> Result<SpeakingGuard, String> {
    let err = |e: JsValue| format!("Web Audio unavailable: {e:?}");
    let context = web_sys::AudioContext::new().map_err(err)?;
    // Autoplay policies may hand us a suspended context; resume is a
    // no-op when it is already running. Best-effort: a suspended context
    // simply never reports speech rather than failing the call.
    if let Ok(promise) = context.resume() {
        let _ = JsFuture::from(promise).await;
    }
    let source = context.create_media_stream_source(stream).map_err(err)?;
    let analyser = context.create_analyser().map_err(err)?;
    analyser.set_fft_size(SPEAKING_WINDOW as u32);
    let sink = context.create_gain().map_err(err)?;
    sink.gain().set_value(0.0);

    source.connect_with_audio_node(&analyser).map_err(err)?;
    analyser.connect_with_audio_node(&sink).map_err(err)?;
    sink.connect_with_audio_node(&context.destination())
        .map_err(err)?;

    let mut window = vec![0f32; SPEAKING_WINDOW];
    let mut speaking = false;
    let mut quiet_ticks: u32 = 0;
    let analyser_for_timer = analyser.clone();
    let callback = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
        analyser_for_timer.get_float_time_domain_data(&mut window);
        let sum: f32 = window.iter().map(|s| s * s).sum();
        let rms = (sum / window.len() as f32).sqrt();
        if rms > SPEAKING_RMS {
            quiet_ticks = 0;
            if !speaking {
                speaking = true;
                on_change(true);
            }
        } else {
            quiet_ticks = quiet_ticks.saturating_add(1);
            if speaking && quiet_ticks >= SPEAKING_HANGOVER_TICKS {
                speaking = false;
                on_change(false);
            }
        }
    });

    let window_handle = web_sys::window().ok_or_else(|| "no window".to_owned())?;
    let interval_handle = window_handle
        .set_interval_with_callback_and_timeout_and_arguments_0(
            callback.as_ref().unchecked_ref::<js_sys::Function>(),
            SPEAKING_POLL_MS,
        )
        .map_err(err)?;

    Ok(SpeakingGuard {
        _interval_handle: interval_handle,
        _callback: callback,
        _nodes: (context, source, analyser, sink),
    })
}

/// Acquire a screen-capture stream and return it with its video track.
///
/// The e2e harness (`?test=1`) runs on a headless display with nothing to
/// capture, so there the fake camera stands in for a screen; real users go
/// through the browser's `getDisplayMedia` picker.
pub async fn request_screen() -> Result<(MediaStream, MediaStreamTrack), String> {
    let test_mode = web_sys::window()
        .and_then(|w| w.location().search().ok())
        .is_some_and(|s| s.contains("test=1"));
    let stream = if test_mode {
        // The audio constraints are irrelevant for the screen stand-in
        // (only the video track is used), so plain defaults go in.
        request_media(None, None, AudioSettings::default()).await?
    } else {
        let devices = media_devices()?;
        let promise = devices
            .get_display_media()
            .map_err(|e| format!("cannot call getDisplayMedia: {e:?}"))?;
        let value = JsFuture::from(promise)
            .await
            .map_err(|e| format!("screen capture denied or failed: {e:?}"))?;
        value
            .dyn_into::<MediaStream>()
            .map_err(|e| format!("getDisplayMedia returned {e:?}"))?
    };
    let track = tracks_of(&stream)
        .into_iter()
        .find(|t| t.kind() == "video")
        .ok_or_else(|| "screen capture has no video track".to_owned())?;
    Ok((stream, track))
}

/// Fires `on_end` when `track` ends — the browser's own stop-sharing
/// button ends a capture track, and the UI must wind the share down.
pub struct TrackEndGuard {
    track: MediaStreamTrack,
    _on_ended: wasm_bindgen::closure::Closure<dyn FnMut()>,
}

impl Drop for TrackEndGuard {
    fn drop(&mut self) {
        // Detach before the closure drops: a late `ended` event firing
        // into a dropped closure panics the wasm module.
        self.track.set_onended(None);
    }
}

/// Watch `track` for the browser-native end signal. Keep the returned
/// guard alive as long as the share is live.
pub fn watch_track_ended<F: FnMut() + 'static>(
    track: &MediaStreamTrack,
    on_end: F,
) -> TrackEndGuard {
    let closure = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(on_end);
    track.set_onended(Some(closure.as_ref().unchecked_ref::<js_sys::Function>()));
    TrackEndGuard {
        track: track.clone(),
        _on_ended: closure,
    }
}

fn media_devices() -> Result<MediaDevices, String> {
    navigator()?
        .media_devices()
        .map_err(|e| format!("no media devices: {e:?}"))
}

fn navigator() -> Result<Navigator, String> {
    let window = web_sys::window().ok_or("no window".to_owned())?;
    Ok(window.navigator())
}

/// The getUserMedia constraint shape `{ deviceId: { exact: id } }`.
fn exact_device(id: &str) -> JsValue {
    let exact = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&exact, &JsValue::from_str("exact"), &JsValue::from_str(id));
    let constraint = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &constraint,
        &JsValue::from_str("deviceId"),
        &JsValue::from(exact),
    );
    constraint.into()
}
