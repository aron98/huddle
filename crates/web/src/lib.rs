#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![allow(async_fn_in_trait)] // our boundary traits are used generically, never dyn/Send

//! The huddle web client.
//!
//! Leptos CSR app: the prejoin page and (later) the in-call UI.

#[cfg(target_arch = "wasm32")]
pub mod media;
pub mod rtc;

use leptos::prelude::*;
#[cfg(target_arch = "wasm32")]
use leptos::task::spawn_local;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

#[cfg(target_arch = "wasm32")]
use rtc::{BrowserPeerConnection, BrowserSignalingStream, RtcClient};

/// Application root. The URL decides the page:
/// `/` → landing, `/m/{code}` → the prejoin page for that meeting.
#[component]
pub fn App() -> impl IntoView {
    let path = window().location().pathname().unwrap_or_default();
    let code = path
        .strip_prefix("/m/")
        .map(|c| c.trim_end_matches('/'))
        .filter(|c| !c.is_empty());

    match code {
        Some(code) => view! { <Prejoin code={code.to_owned()} /> }.into_any(),
        None => view! { <Landing /> }.into_any(),
    }
}

#[component]
fn Landing() -> impl IntoView {
    view! {
        <main>
            <h1>"huddle"</h1>
            <p>"Lightweight self-hosted video meetings."</p>
            <p>
                "Open a meeting link like "
                <code>"/m/H7KD-PM2X-QR4T"</code>
                " to join."
            </p>
        </main>
    }
}

// On native (test) builds the prejoin page stays a placeholder: the browser
// glue is wasm-only, and the signaling driver is tested via the mock server.
#[cfg(not(target_arch = "wasm32"))]
#[component]
fn Prejoin(code: String) -> impl IntoView {
    view! {
        <main>
            <h1>"Join meeting"</h1>
            <p>{format!("Meeting code: {code}")}</p>
        </main>
    }
}

/// The page a guest lands on from a meeting link: name, camera preview,
/// device pickers, and the join button.
#[cfg(target_arch = "wasm32")]
#[component]
fn Prejoin(code: String) -> impl IntoView {
    use web_sys::MediaStream;

    let name = RwSignal::new(String::new());
    let stream = RwSignal::<Option<MediaStream>>::new(None);
    let devices = RwSignal::<Vec<media::Device>>::new(Vec::new());
    let camera_id = RwSignal::<Option<String>>::new(None);
    let mic_id = RwSignal::<Option<String>>::new(None);
    let cam_on = RwSignal::new(true);
    let mic_on = RwSignal::new(true);
    let status = RwSignal::<Option<String>>::new(None);

    // Grab the default camera/mic on mount, then list devices (labels only
    // appear once permission is granted).
    spawn_local(async move {
        match media::request_media(None, None).await {
            Ok(s) => stream.set(Some(s)),
            Err(e) => status.set(Some(format!("Camera/mic unavailable: {e}"))),
        }
        match media::list_devices().await {
            Ok(found) => devices.set(found),
            Err(e) => status.set(Some(e)),
        }
    });

    // Show the acquired stream in the preview element.
    Effect::new(move |_| {
        let Some(s) = stream.get() else { return };
        let Some(element) = document().get_element_by_id("preview") else {
            return;
        };
        let Ok(video) = element.dyn_into::<web_sys::HtmlVideoElement>() else {
            return;
        };
        video.set_muted(true);
        video.set_src_object(Some(&s));
        let _ = video.play();
    });

    // Re-acquire media when a device is picked.
    fn reacquire(
        which: &'static str,
        id: Option<String>,
        camera_id: RwSignal<Option<String>>,
        mic_id: RwSignal<Option<String>>,
        stream: RwSignal<Option<MediaStream>>,
        status: RwSignal<Option<String>>,
    ) {
        if which == "camera" {
            camera_id.set(id.clone());
        } else {
            mic_id.set(id.clone());
        }
        status.set(None);
        spawn_local(async move {
            match media::request_media(
                camera_id.get_untracked().as_deref(),
                mic_id.get_untracked().as_deref(),
            )
            .await
            {
                Ok(s) => stream.set(Some(s)),
                Err(e) => status.set(Some(format!("{which} unavailable: {e}"))),
            }
        });
    }

    let on_camera_change = move |ev| {
        let id = event_target_value(&ev);
        let id = if id.is_empty() { None } else { Some(id) };
        reacquire("camera", id, camera_id, mic_id, stream, status);
    };
    let on_mic_change = move |ev| {
        let id = event_target_value(&ev);
        let id = if id.is_empty() { None } else { Some(id) };
        reacquire("mic", id, camera_id, mic_id, stream, status);
    };

    let toggle_cam = move |_| {
        cam_on.update(|v| *v = !*v);
        if let Some(s) = stream.get_untracked() {
            for track in media::tracks_of(&s) {
                track.set_enabled(cam_on.get_untracked());
            }
        }
    };
    let toggle_mic = move |_| {
        mic_on.update(|v| *v = !*v);
        if let Some(s) = stream.get_untracked() {
            for track in media::tracks_of(&s) {
                track.set_enabled(mic_on.get_untracked());
            }
        }
    };

    let cameras = Memo::new(move |_| {
        devices
            .get()
            .into_iter()
            .filter(|d| d.kind == media::DeviceKind::Camera)
            .collect::<Vec<_>>()
    });
    let microphones = Memo::new(move |_| {
        devices
            .get()
            .into_iter()
            .filter(|d| d.kind == media::DeviceKind::Microphone)
            .collect::<Vec<_>>()
    });

    // Join: open signaling, attach media, and run the join + publish
    // handshake. The in-call UI lands in phase 2.
    let join_code = code.clone();
    let join = move |_| {
        if name.get().trim().is_empty() {
            status.set(Some("Enter your name first.".to_owned()));
            return;
        }
        status.set(Some("Connecting…".to_owned()));
        let code = join_code.clone();
        let name = name.get_untracked();
        let local = stream.get_untracked();
        spawn_local(async move {
            match join_meeting(&code, name.trim(), local.as_ref()).await {
                Ok(()) => status.set(Some(
                    "Connected (the in-call UI lands in phase 2).".to_owned(),
                )),
                Err(e) => status.set(Some(e)),
            }
        });
    };

    view! {
        <main>
            <h1>"Join meeting"</h1>
            <p>{format!("Meeting code: {code}")}</p>

            <video id="preview" autoplay muted playsinline></video>

            <label>
                "Your name"
                <input
                    type="text"
                    placeholder="e.g. Alice"
                    prop:value=name
                    on:input=move |ev| name.set(event_target_value(&ev))
                />
            </label>

            <label>
                "Camera"
                <select on:change=on_camera_change>
                    <option value="">"Default"</option>
                    <For each={move || cameras.get()} key=|d| d.id.clone() let:d>
                        <option
                            value={d.id.clone()}
                            selected={move || camera_id.get().as_deref() == Some(d.id.as_str())}
                        >
                            {d.label.clone()}
                        </option>
                    </For>
                </select>
            </label>

            <label>
                "Microphone"
                <select on:change=on_mic_change>
                    <option value="">"Default"</option>
                    <For each={move || microphones.get()} key=|d| d.id.clone() let:d>
                        <option
                            value={d.id.clone()}
                            selected={move || mic_id.get().as_deref() == Some(d.id.as_str())}
                        >
                            {d.label.clone()}
                        </option>
                    </For>
                </select>
            </label>

            <div>
                <button on:click=toggle_cam>
                    {move || if cam_on.get() { "Camera on" } else { "Camera off" }}
                </button>
                <button on:click=toggle_mic>
                    {move || if mic_on.get() { "Mic on" } else { "Mic off" }}
                </button>
            </div>

            <button on:click=join>"Join"</button>

            {move || status.get().map(|s| view! { <p role="status">{s}</p> })}
        </main>
    }
}

/// Open signaling and run the join + publish handshake against `/ws`.
/// The server-side room is phase 2; for now failures surface as status text.
#[cfg(target_arch = "wasm32")]
async fn join_meeting(
    code: &str,
    name: &str,
    local: Option<&web_sys::MediaStream>,
) -> Result<(), String> {
    let host = window()
        .location()
        .host()
        .unwrap_or_else(|_| "localhost".to_owned());
    let url = format!("ws://{host}/ws");

    let signaling = BrowserSignalingStream::connect(&url).map_err(|e| e.to_string())?;
    let peer = BrowserPeerConnection::new(&[]).map_err(|e| e.to_string())?;
    if let Some(stream) = local {
        peer.attach_stream(stream);
    }

    let mut client = RtcClient::new(signaling, peer);
    client.join(code, name).await.map_err(|e| e.to_string())?;
    client.publish().await.map_err(|e| e.to_string())?;
    Ok(())
}
