//! Camera/microphone acquisition and device enumeration.

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
pub async fn request_media(
    camera_id: Option<&str>,
    mic_id: Option<&str>,
) -> Result<MediaStream, String> {
    let devices = media_devices()?;

    let constraints = MediaStreamConstraints::new();
    match mic_id {
        Some(id) => constraints.set_audio(&exact_device(id)),
        None => constraints.set_audio_bool(true),
    }
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
