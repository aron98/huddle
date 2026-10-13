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

/// Consume a media-play promise so a late rejection (e.g. the tile was
/// unmounted mid-play) never surfaces as an unhandled page error.
#[cfg(target_arch = "wasm32")]
fn play_silently(video: &web_sys::HtmlVideoElement) {
    if let Ok(promise) = video.play() {
        spawn_local(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        });
    }
}
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
        play_silently(&video);
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

    // Join hands the meeting over to the in-call view: the button just
    // records who we are; `CallView` opens signaling and runs the session.
    let join_code = code.clone();
    let call = RwSignal::<Option<(String, Option<web_sys::MediaStream>)>>::new(None);
    let join = move |_| {
        if name.get().trim().is_empty() {
            status.set(Some("Enter your name first.".to_owned()));
            return;
        }
        status.set(None);
        call.set(Some((
            name.get_untracked().trim().to_owned(),
            stream.get_untracked(),
        )));
    };

    let prejoin_form = move || {
        view! {
            <>
                <h1>"Join meeting"</h1>
                <p>{format!("Meeting code: {code}")}</p>

            <video id="preview" autoplay muted playsinline></video>

            <label>
                "Your name"
                <input
                    type="text"
                    placeholder="e.g. Alice"
                    data-testid="name-input"
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

            <button data-testid="join-btn" on:click=join>"Join"</button>

            {move || status.get().map(|s| view! { <p role="status">{s}</p> })}
            </>
        }
    };

    view! {
        <main>
            {move || match call.get() {
                None => prejoin_form().into_any(),
                Some((joined_name, local)) => view! {
                    <CallView code=join_code.clone() name=joined_name local=local />
                }
                .into_any(),
            }}
        </main>
    }
}

/// What the in-call controls ask the session task to do.
#[cfg(target_arch = "wasm32")]
enum UiAction {
    Mute {
        audio_muted: Option<bool>,
        video_muted: Option<bool>,
    },
    Leave,
}

/// The in-call page: media tiles, roster, and controls. One spawned task
/// owns the [`RtcClient`]; button clicks travel to it over a channel and
/// server messages flow back through the signals.
#[cfg(target_arch = "wasm32")]
#[component]
fn CallView(code: String, name: String, local: Option<web_sys::MediaStream>) -> impl IntoView {
    use futures::future::FutureExt;
    use futures::stream::StreamExt;
    use huddle_protocol::Participant;
    use uuid::Uuid;
    use web_sys::MediaStream;

    let remote_streams = RwSignal::<Vec<MediaStream>>::new(Vec::new());
    let roster = RwSignal::<Vec<Participant>>::new(Vec::new());
    let status = RwSignal::<Option<String>>::new(Some("Connecting…".to_owned()));
    let mic_on = RwSignal::new(true);
    let cam_on = RwSignal::new(true);

    let (ui_tx, mut ui_rx) = futures::channel::mpsc::unbounded::<UiAction>();

    // The session task: connect, join, publish, then pump messages and UI
    // actions until the call ends.
    let task_code = code.clone();
    let task_name = name.clone();
    let task_local = local.clone();
    spawn_local(async move {
        let code = task_code;
        let name = task_name;
        let local = task_local;
        let host = window()
            .location()
            .host()
            .unwrap_or_else(|_| "localhost".to_owned());
        let signaling = match BrowserSignalingStream::connect(&format!("ws://{host}/ws")).await {
            Ok(s) => s,
            Err(e) => {
                status.set(Some(format!("Cannot reach the server: {e}")));
                return;
            }
        };
        let peer = match BrowserPeerConnection::new(&[]) {
            Ok(p) => p,
            Err(e) => {
                status.set(Some(format!("WebRTC unavailable: {e}")));
                return;
            }
        };

        // Test hook: let e2e reach the peer connection via getStats.
        if window()
            .location()
            .search()
            .unwrap_or_default()
            .contains("test=1")
        {
            let _ = js_sys::Reflect::set(
                &window().into(),
                &wasm_bindgen::JsValue::from_str("__huddlePc"),
                &wasm_bindgen::JsValue::from(peer.raw()),
            );
        }

        // The SFU labels each forwarded stream with the origin
        // participant's id as the msid.
        let streams = remote_streams;
        peer.set_on_remote_stream(move |stream| streams.update(|list| list.push(stream)));
        if let Some(local) = &local {
            peer.attach_stream(local);
        }

        let mut client = RtcClient::new(signaling, peer);
        if let Err(e) = client.join(&code, &name).await {
            status.set(Some(format!("Join failed: {e}")));
            return;
        }
        roster.set(client.roster().to_vec());
        if let Err(e) = client.publish().await {
            status.set(Some(format!("Publish failed: {e}")));
            return;
        }
        status.set(None);

        loop {
            futures::select! {
                message = client.next_server_message().fuse() => {
                    match message {
                        Ok(_) => roster.set(client.roster().to_vec()),
                        Err(e) => {
                            status.set(Some(format!("Disconnected: {e}")));
                            break;
                        }
                    }
                }
                action = ui_rx.select_next_some() => {
                    match action {
                        UiAction::Mute { audio_muted, video_muted } => {
                            if let Err(e) = client.set_mute_state(audio_muted, video_muted).await {
                                status.set(Some(format!("Disconnected: {e}")));
                                break;
                            }
                        }
                        UiAction::Leave => {
                            let _ = client.leave().await;
                            status.set(Some("You left the meeting.".to_owned()));
                            break;
                        }
                    }
                }
            }
        }
    });

    let toggle_mic = {
        let local = local.clone();
        let ui_tx = ui_tx.clone();
        move |_| {
            mic_on.update(|v| *v = !*v);
            let muted = !mic_on.get_untracked();
            if let Some(s) = &local {
                for track in media::tracks_of(s) {
                    if track.kind() == "audio" {
                        track.set_enabled(!muted);
                    }
                }
            }
            let _ = ui_tx.unbounded_send(UiAction::Mute {
                audio_muted: Some(muted),
                video_muted: None,
            });
        }
    };
    let toggle_cam = {
        let local = local.clone();
        let ui_tx = ui_tx.clone();
        move |_| {
            cam_on.update(|v| *v = !*v);
            let muted = !cam_on.get_untracked();
            if let Some(s) = &local {
                for track in media::tracks_of(s) {
                    if track.kind() == "video" {
                        track.set_enabled(!muted);
                    }
                }
            }
            let _ = ui_tx.unbounded_send(UiAction::Mute {
                audio_muted: None,
                video_muted: Some(muted),
            });
        }
    };
    let leave = {
        let ui_tx = ui_tx.clone();
        move |_| {
            let _ = ui_tx.unbounded_send(UiAction::Leave);
        }
    };

    // Resolve a remote stream's origin participant from its msid.
    fn label_for(stream: &MediaStream, roster: &[Participant]) -> String {
        Uuid::parse_str(&stream.id())
            .ok()
            .and_then(|id| roster.iter().find(|p| p.id == id))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Guest".to_owned())
    }

    view! {
        <main>
            <h1>{format!("Meeting {code}")}</h1>

            {move || status.get().map(|s| view! { <p data-testid="call-status" role="status">{s}</p> })}

            <div class="tiles">
                {local
                    .clone()
                    .map(|stream| {
                        view! {
                            <VideoTile
                                stream=stream
                                label="You".to_string()
                                muted=true
                                testid="tile-you".to_string()
                            />
                        }
                    })}

                <For each={move || remote_streams.get()} key=|s| s.id() let:s>
                    {move || {
                        let label = label_for(&s, &roster.get());
                        let testid = format!("tile-{label}");
                        view! {
                            <VideoTile
                                stream=s.clone()
                                label=label
                                muted=false
                                testid=testid
                            />
                        }
                    }}
                </For>
            </div>

            <ul data-testid="roster">
                <For each={move || roster.get()} key=|p| p.id let:p>
                    {move || {
                        // Re-read the participant so roster updates re-render.
                        let current = roster
                            .get()
                            .into_iter()
                            .find(|q| q.id == p.id)
                            .unwrap_or_else(|| p.clone());
                        let testid = format!("roster-{}", current.name);
                        let mic = if current.audio_muted { "mic off" } else { "mic on" };
                        let cam = if current.video_muted { "cam off" } else { "cam on" };
                        let text = format!("{} ({mic}, {cam})", current.name);
                        view! {
                            <li data-testid=testid>{text}</li>
                        }
                    }}
                </For>
            </ul>

            <div class="controls">
                <button data-testid="toggle-mic" on:click=toggle_mic>
                    {move || if mic_on.get() { "Mic on" } else { "Mic off" }}
                </button>
                <button data-testid="toggle-cam" on:click=toggle_cam>
                    {move || if cam_on.get() { "Camera on" } else { "Camera off" }}
                </button>
                <button data-testid="leave-btn" on:click=leave>"Leave"</button>
            </div>
        </main>
    }
}

/// One media tile: a `<video>` bound to `stream` with a caption.
#[cfg(target_arch = "wasm32")]
#[component]
fn VideoTile(
    stream: web_sys::MediaStream,
    label: String,
    muted: bool,
    testid: String,
) -> impl IntoView {
    let video_ref = NodeRef::<leptos::html::Video>::new();
    Effect::new(move |_| {
        if let Some(video) = video_ref.get() {
            video.set_muted(muted);
            video.set_src_object(Some(&stream));
            play_silently(&video);
        }
    });
    view! {
        <figure class="tile" data-testid=testid>
            <video node_ref=video_ref autoplay muted playsinline></video>
            <figcaption>{label}</figcaption>
        </figure>
    }
}
