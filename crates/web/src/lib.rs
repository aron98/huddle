#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![allow(async_fn_in_trait)] // our boundary traits are used generically, never dyn/Send

//! The huddle web client.
//!
//! Leptos CSR app: the prejoin page and (later) the in-call UI.

#[cfg(target_arch = "wasm32")]
pub mod api;
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
        None => match path.as_str() {
            #[cfg(target_arch = "wasm32")]
            "/login" => view! { <Login /> }.into_any(),
            #[cfg(target_arch = "wasm32")]
            "/meetings" => view! { <Dashboard /> }.into_any(),
            _ => view! { <Landing /> }.into_any(),
        },
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
            <p>
                <a href="/login" data-testid="nav-login">"Sign in"</a>
                " · "
                <a href="/meetings" data-testid="nav-meetings">"My meetings"</a>
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

    // Re-acquire media when a device is picked. Fresh getUserMedia
    // tracks always come back enabled, so the toggle state is re-applied
    // to the new stream: switching the mic must not resurrect a muted
    // track, and it must not disable a live camera either.
    fn reacquire(
        which: &'static str,
        id: Option<String>,
        camera_id: RwSignal<Option<String>>,
        mic_id: RwSignal<Option<String>>,
        cam_on: RwSignal<bool>,
        mic_on: RwSignal<bool>,
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
                Ok(s) => {
                    media::set_track_kind_enabled(&s, "audio", mic_on.get_untracked());
                    media::set_track_kind_enabled(&s, "video", cam_on.get_untracked());
                    stream.set(Some(s));
                }
                Err(e) => status.set(Some(format!("{which} unavailable: {e}"))),
            }
        });
    }

    let on_camera_change = move |ev| {
        let id = event_target_value(&ev);
        let id = if id.is_empty() { None } else { Some(id) };
        reacquire(
            "camera", id, camera_id, mic_id, cam_on, mic_on, stream, status,
        );
    };
    let on_mic_change = move |ev| {
        let id = event_target_value(&ev);
        let id = if id.is_empty() { None } else { Some(id) };
        reacquire("mic", id, camera_id, mic_id, cam_on, mic_on, stream, status);
    };

    // Each toggle touches only its own track kind — a mic-off must never
    // black out the camera preview (issue #7).
    let toggle_cam = move |_| {
        cam_on.update(|v| *v = !*v);
        if let Some(s) = stream.get_untracked() {
            media::set_track_kind_enabled(&s, "video", cam_on.get_untracked());
        }
    };
    let toggle_mic = move |_| {
        mic_on.update(|v| *v = !*v);
        if let Some(s) = stream.get_untracked() {
            media::set_track_kind_enabled(&s, "audio", mic_on.get_untracked());
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
                <select data-testid="camera-select" on:change=on_camera_change>
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
                <select data-testid="mic-select" on:change=on_mic_change>
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
                <button data-testid="prejoin-toggle-cam" on:click=toggle_cam>
                    {move || if cam_on.get() { "Camera on" } else { "Camera off" }}
                </button>
                <button data-testid="prejoin-toggle-mic" on:click=toggle_mic>
                    {move || if mic_on.get() { "Mic on" } else { "Mic off" }}
                </button>
            </div>

            // A phase-2 call is media-first: until getUserMedia has
            // produced a local stream there is nothing to publish, and
            // an offer with zero m-lines would be rejected by the SFU.
            <button
                data-testid="join-btn"
                disabled=move || stream.get().is_none()
                on:click=join
            >
                {move || if stream.get().is_some() { "Join meeting" } else { "Getting camera and mic…" }}
            </button>

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
    Speaking(bool),
    ShareScreen(ScreenCommand),
    Chat {
        body: String,
    },
    Leave,
}

/// Which direction the share-screen flow goes.
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy)]
enum ScreenCommand {
    Start,
    Stop,
}

/// The in-call page: media tiles, roster, and controls. One spawned task
/// owns the [`RtcClient`]; button clicks travel to it over a channel and
/// server messages flow back through the signals.
#[cfg(target_arch = "wasm32")]
#[component]
fn CallView(code: String, name: String, local: Option<web_sys::MediaStream>) -> impl IntoView {
    use futures::future::FutureExt;
    use futures::stream::StreamExt;
    use huddle_protocol::{Participant, ServerMessage};
    use uuid::Uuid;
    use web_sys::MediaStream;

    let remote_streams = RwSignal::<Vec<(String, MediaStream)>>::new(Vec::new());
    let roster = RwSignal::<Vec<Participant>>::new(Vec::new());
    let speaking = RwSignal::<std::collections::HashSet<Uuid>>::new(Default::default());
    let status = RwSignal::<Option<String>>::new(Some("Connecting…".to_owned()));
    // The prejoin toggles survive the handover: the local stream is the
    // same object, so each track's `enabled` state is the truth to seed
    // the buttons from — muted on prejoin renders "Mic off" in-call.
    let mic_on = RwSignal::new(
        local
            .as_ref()
            .map(|s| media::track_kind_enabled(s, "audio"))
            .unwrap_or(true),
    );
    let cam_on = RwSignal::new(
        local
            .as_ref()
            .map(|s| media::track_kind_enabled(s, "video"))
            .unwrap_or(true),
    );
    let sharing = RwSignal::new(false);
    // Chat messages (live relay + history replay) shown in the sidebar.
    let chat = RwSignal::<Vec<huddle_protocol::ChatMessage>>::new(Vec::new());
    let chat_draft = RwSignal::new(String::new());

    let (ui_tx, mut ui_rx) = futures::channel::mpsc::unbounded::<UiAction>();
    // The session task reports our own speaking through the same channel.
    let task_ui_tx = ui_tx.clone();

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
        // Test knob: `?turn=relay` forces every candidate through the
        // TURN relay (`iceTransportPolicy: "relay"`). The relay servers
        // themselves arrive with the Welcome and are applied inside
        // `RtcClient::join`.
        let relay = window()
            .location()
            .search()
            .unwrap_or_default()
            .contains("turn=relay");
        let peer = match if relay {
            BrowserPeerConnection::new_relay(&[])
        } else {
            BrowserPeerConnection::new(&[])
        } {
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
        // participant's id as the msid. Our own publish is answered
        // recvonly, but Chrome still creates receivers for it — skip that
        // self-echo so it does not render as a phantom "Guest" tile.
        let streams = remote_streams;
        let local_id = local.as_ref().map(|s| s.id());
        peer.set_on_remote_stream(move |id, stream| {
            if local_id.as_deref() == Some(id) {
                return;
            }
            streams.update(|list| list.push((id.to_owned(), stream)));
        });
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

        // A prejoin mute must survive into the call for everyone: the
        // roster's canonical state lives on the server, which assumes a
        // fresh joiner is live, so mirror the actual track state once
        // media is up. Only a muted track needs the correction.
        if let Some(local) = &local {
            let audio_muted = !media::track_kind_enabled(local, "audio");
            let video_muted = !media::track_kind_enabled(local, "video");
            if (audio_muted || video_muted)
                && let Err(e) = client
                    .set_mute_state(Some(audio_muted), Some(video_muted))
                    .await
            {
                status.set(Some(format!("Disconnected: {e}")));
                return;
            }
        }

        // The screen-share flow reports the browser's own stop-sharing
        // signal (track ended) back into the session through the same
        // channel the buttons use; clone the sender before the speaking
        // watcher below moves it into its closure.
        let screen_ui_tx = task_ui_tx.clone();

        // Best-effort speaking detection: watch our own level and report
        // flips through the session. Failure is non-fatal — the call just
        // runs without the indicator.
        let _speaking_watch = match local.as_ref() {
            Some(s) => match media::watch_speaking(s, move |speaking| {
                let _ = task_ui_tx.unbounded_send(UiAction::Speaking(speaking));
            })
            .await
            {
                Ok(guard) => Some(guard),
                Err(e) => {
                    leptos::logging::warn!("speaking detection off: {e}");
                    None
                }
            },
            None => None,
        };

        // Keeps the browser-native stop-sharing handler alive while the
        // share is live; dropped when we stop (or when a new share starts).
        let mut screen_end_guard: Option<media::TrackEndGuard> = None;

        loop {
            // Read inside the select but handle OUTSIDE it: handling a server
            // message awaits browser promises and sends the SDP answer, so a
            // dropped future (e.g. a speaking flip winning the select mid-way)
            // would leave the answer unsent and wedge the peer's negotiation
            // forever. `read_server_message` is cancel-safe; the handling
            // future below runs to completion.
            futures::select! {
                message = client.read_server_message().fuse() => match message {
                    Ok(msg) => {
                        if let Err(e) = client.handle_server_message(msg.clone()).await {
                            status.set(Some(format!("Disconnected: {e}")));
                            break;
                        }
                        // A departed participant's tiles go with them: the
                        // camera stream carries their id, the screen
                        // stream carries `{id}-screen`.
                        if let ServerMessage::ParticipantLeft { id } = &msg {
                            let gone = id.to_string();
                            let screen = format!("{gone}-screen");
                            remote_streams
                                .update(|list| list.retain(|(sid, _)| sid != &gone && sid != &screen));
                        }
                        // A stopped share withdraws the owner's screen tile.
                        if let ServerMessage::Sharing { id, sharing: false } = &msg {
                            let screen = format!("{id}-screen");
                            remote_streams
                                .update(|list| list.retain(|(sid, _)| sid != &screen));
                        }
                        roster.set(client.roster().to_vec());
                        speaking.set(client.speaking().clone());
                        chat.set(client.chat_log().to_vec());
                    }
                    Err(e) => {
                        status.set(Some(format!("Disconnected: {e}")));
                        break;
                    }
                },
                action = ui_rx.select_next_some() => {
                    match action {
                        UiAction::Mute { audio_muted, video_muted } => {
                            if let Err(e) = client.set_mute_state(audio_muted, video_muted).await {
                                status.set(Some(format!("Disconnected: {e}")));
                                break;
                            }
                        }
                        UiAction::Speaking(speaking) => {
                            if let Err(e) = client.set_speaking(speaking).await {
                                status.set(Some(format!("Disconnected: {e}")));
                                break;
                            }
                        }
                        UiAction::ShareScreen(command) => match command {
                            ScreenCommand::Start => {
                                // Acquire the capture first: a denied
                                // picker is a UI hiccup, not a call ender.
                                match media::request_screen().await {
                                    Ok((stream, track)) => {
                                        client
                                            .peer_mut()
                                            .screen_share_handle()
                                            .stage(track.clone(), stream);
                                        let stop_tx = screen_ui_tx.clone();
                                        let guard = media::watch_track_ended(&track, move || {
                                            let _ = stop_tx.unbounded_send(
                                                UiAction::ShareScreen(ScreenCommand::Stop),
                                            );
                                        });
                                        // Reading the old guard via `replace` also
                                        // drops it — one share watcher at a time.
                                        screen_end_guard.replace(guard);
                                        match client.start_screen_share().await {
                                            Ok(()) => sharing.set(true),
                                            Err(e) => {
                                                screen_end_guard.take();
                                                sharing.set(false);
                                                status.set(Some(format!("Screen share failed: {e}")));
                                                break;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        status.set(Some(format!("Screen share unavailable: {e}")));
                                    }
                                }
                            }
                            ScreenCommand::Stop => {
                                // Dropping the watcher detaches `onended`; the
                                // task keeps no share state of its own.
                                screen_end_guard.take();
                                sharing.set(false);
                                if let Err(e) = client.stop_screen_share().await {
                                    status.set(Some(format!("Disconnected: {e}")));
                                    break;
                                }
                            }
                        },
                        UiAction::Chat { body } => {
                            if let Err(e) = client.send_chat(body).await {
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
                media::set_track_kind_enabled(s, "audio", !muted);
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
                media::set_track_kind_enabled(s, "video", !muted);
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
    let share = {
        let ui_tx = ui_tx.clone();
        move |_| {
            let command = if sharing.get_untracked() {
                ScreenCommand::Stop
            } else {
                ScreenCommand::Start
            };
            let _ = ui_tx.unbounded_send(UiAction::ShareScreen(command));
        }
    };

    // Send the chat draft, shared by the button and the Enter key.
    fn submit_chat_body(
        chat_draft: RwSignal<String>,
        ui_tx: &futures::channel::mpsc::UnboundedSender<UiAction>,
    ) {
        let body = chat_draft.get_untracked().trim().to_owned();
        if body.is_empty() {
            return;
        }
        chat_draft.set(String::new());
        let _ = ui_tx.unbounded_send(UiAction::Chat { body });
    }
    let on_send_click = {
        let ui_tx = ui_tx.clone();
        move |_: web_sys::MouseEvent| submit_chat_body(chat_draft, &ui_tx)
    };
    let on_send_key = {
        let ui_tx = ui_tx.clone();
        move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "Enter" {
                submit_chat_body(chat_draft, &ui_tx);
            }
        }
    };

    // Resolve a remote stream's origin participant from its msid. The id
    // string was captured at `ontrack` time; never re-read `.id()` from
    // the stream (unstable in Chrome, see `BrowserPeerConnection::new`).
    fn label_for(id: &str, roster: &[Participant]) -> String {
        Uuid::parse_str(id)
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
                                speaking=false
                                screen=false
                            />
                        }
                    })}

                <For each={move || remote_streams.get()} key=|s| s.0.clone() let:s>
                    {move || {
                        // The SFU labels forwarded camera streams with the
                        // origin participant's id and screen streams with
                        // `{id}-screen`, so the suffix decides the tile.
                        let (origin, is_screen) = match s.0.strip_suffix("-screen") {
                            Some(origin) => (origin, true),
                            None => (s.0.as_str(), false),
                        };
                        let base = label_for(origin, &roster.get());
                        // The speaking set is keyed by participant id, so
                        // it tells us whose camera tile to ring.
                        let is_speaking = !is_screen
                            && Uuid::parse_str(origin)
                                .is_ok_and(|id| speaking.get().contains(&id));
                        let (label, testid) = if is_screen {
                            (format!("{base}'s screen"), format!("tile-{base}-screen"))
                        } else {
                            (base.clone(), format!("tile-{base}"))
                        };
                        view! {
                            <VideoTile
                                stream=s.1.clone()
                                label=label
                                muted=false
                                testid=testid
                                speaking=is_speaking
                                screen=is_screen
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
                        let text = if current.sharing {
                            format!("{} ({mic}, {cam}, sharing)", current.name)
                        } else {
                            format!("{} ({mic}, {cam})", current.name)
                        };
                        view! {
                            <li data-testid=testid>{text}</li>
                        }
                    }}
                </For>
            </ul>

            <aside class="chat">
                <h2>"Chat"</h2>
                <ul data-testid="chat-log">
                    <For each={move || chat.get()} key=|m| m.id let:m>
                        {move || {
                            view! {
                                <li class={if m.sender_id.is_none() { "system" } else { "" }}>
                                    <strong>{m.sender_name.clone()}</strong>
                                    " "
                                    {m.body.clone()}
                                </li>
                            }
                        }}
                    </For>
                </ul>
                <input
                    type="text"
                    placeholder="Message everyone"
                    data-testid="chat-input"
                    prop:value=chat_draft
                    on:input=move |ev| chat_draft.set(event_target_value(&ev))
                    on:keydown=on_send_key
                />
                <button data-testid="chat-send" on:click=on_send_click>"Send"</button>
            </aside>

            <div class="controls">
                <button data-testid="toggle-mic" on:click=toggle_mic>
                    {move || if mic_on.get() { "Mic on" } else { "Mic off" }}
                </button>
                <button data-testid="toggle-cam" on:click=toggle_cam>
                    {move || if cam_on.get() { "Camera on" } else { "Camera off" }}
                </button>
                <button data-testid="share-screen" on:click=share>
                    {move || if sharing.get() { "Stop sharing" } else { "Share screen" }}
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
    speaking: bool,
    screen: bool,
) -> impl IntoView {
    let video_ref = NodeRef::<leptos::html::Video>::new();
    Effect::new(move |_| {
        if let Some(video) = video_ref.get() {
            video.set_muted(muted);
            video.set_src_object(Some(&stream));
            play_silently(&video);
        }
    });
    let class = match (speaking, screen) {
        (_, true) => "tile screen",
        (true, false) => "tile speaking",
        (false, false) => "tile",
    };
    view! {
        <figure class=class data-testid=testid>
            <video node_ref=video_ref autoplay muted playsinline></video>
            <figcaption>{label}</figcaption>
        </figure>
    }
}

/// The organizer sign-in page: request a one-time magic link by email.
#[cfg(target_arch = "wasm32")]
#[component]
fn Login() -> impl IntoView {
    let email = RwSignal::new(String::new());
    let status = RwSignal::<Option<String>>::new(None);

    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let address = email.get_untracked().trim().to_owned();
        if address.is_empty() {
            status.set(Some("Enter your email first.".to_owned()));
            return;
        }
        status.set(Some("Sending…".to_owned()));
        let body = serde_json::json!({ "email": address }).to_string();
        spawn_local(async move {
            match api::fetch_text("POST", "/api/auth/request-link", Some(body)).await {
                Ok((200, _)) => status.set(Some(
                    "Check your email — a sign-in link is on its way. \
                     (Running without SMTP? The link is printed in the server log.)"
                        .to_owned(),
                )),
                Ok((_, text)) => status.set(Some(format!("Could not send the link: {text}"))),
                Err(e) => status.set(Some(format!("Could not send the link: {e}"))),
            }
        });
    };

    view! {
        <main>
            <h1>"Sign in"</h1>
            <p>"We'll email you a one-time sign-in link. No passwords."</p>
            <form on:submit=submit>
                <label>
                    "Email"
                    <input
                        type="email"
                        placeholder="you@example.com"
                        data-testid="login-email"
                        prop:value=email
                        on:input=move |ev| email.set(event_target_value(&ev))
                    />
                </label>
                <button type="submit" data-testid="login-submit">"Email me a link"</button>
            </form>
            {move || {
                status
                    .get()
                    .map(|s| view! { <p data-testid="login-status" role="status">{s}</p> })
            }}
        </main>
    }
}

/// The organizer dashboard: list, create, and manage the meetings you own.
#[cfg(target_arch = "wasm32")]
#[component]
fn Dashboard() -> impl IntoView {
    use huddle_protocol::{CreateMeetingRequest, Joinability, MeetingResponse};
    use time::format_description::well_known::Rfc3339;

    let organizer_email = RwSignal::<Option<String>>::new(None);
    let meetings = RwSignal::<Vec<MeetingResponse>>::new(Vec::new());
    let signed_out = RwSignal::new(false);
    let status = RwSignal::<Option<String>>::new(None);
    let title = RwSignal::new(String::new());
    let starts = RwSignal::new(String::new());
    let ends = RwSignal::new(String::new());

    // Refresh the owned-meetings list in the background.
    fn refresh(meetings: RwSignal<Vec<MeetingResponse>>) {
        spawn_local(async move {
            if let Ok((200, body)) = api::fetch_text("GET", "/api/meetings/mine", None).await
                && let Ok(list) = serde_json::from_str::<Vec<MeetingResponse>>(&body)
            {
                meetings.set(list);
            }
        });
    }

    // Initial load: session first, then the list.
    spawn_local(async move {
        match api::fetch_text("GET", "/api/auth/me", None).await {
            Ok((200, body)) => {
                if let Ok(me) = serde_json::from_str::<serde_json::Value>(&body) {
                    organizer_email.set(me["email"].as_str().map(str::to_owned));
                }
            }
            Ok((401, _)) => signed_out.set(true),
            Ok((_, text)) => status.set(Some(format!("Could not load your session: {text}"))),
            Err(e) => status.set(Some(format!("Could not load your session: {e}"))),
        }
        if !signed_out.get_untracked() {
            refresh(meetings);
        }
    });

    let create = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let parse_time = |raw: &str| {
            api::local_datetime_to_rfc3339(raw)
                .and_then(|s| time::OffsetDateTime::parse(&s, &Rfc3339).ok())
        };
        let starts_at = match parse_time(&starts.get_untracked()) {
            Some(v) => v,
            None => {
                status.set(Some("Pick a start time.".to_owned()));
                return;
            }
        };
        let ends_at = match parse_time(&ends.get_untracked()) {
            Some(v) => v,
            None => {
                status.set(Some("Pick an end time.".to_owned()));
                return;
            }
        };
        let request = CreateMeetingRequest {
            title: title.get_untracked().trim().to_owned(),
            starts_at,
            ends_at,
        };
        if let Err(message) = request.validate() {
            status.set(Some(message.to_owned()));
            return;
        }
        let Ok(body) = serde_json::to_string(&request) else {
            status.set(Some("Could not encode the meeting.".to_owned()));
            return;
        };
        status.set(Some("Creating…".to_owned()));
        spawn_local(async move {
            match api::fetch_text("POST", "/api/meetings", Some(body)).await {
                Ok((201, _)) => {
                    status.set(None);
                    title.set(String::new());
                    starts.set(String::new());
                    ends.set(String::new());
                    refresh(meetings);
                }
                Ok((_, text)) => status.set(Some(format!("Could not create the meeting: {text}"))),
                Err(e) => status.set(Some(format!("Could not create the meeting: {e}"))),
            }
        });
    };

    // Closures that only capture `Copy` signals are themselves `Copy`, so
    // one definition serves every row.
    let cancel = move |code: String| {
        spawn_local(async move {
            match api::fetch_text("DELETE", &format!("/api/meetings/{code}"), None).await {
                Ok((200, _)) => refresh(meetings),
                Ok((_, text)) => status.set(Some(format!("Could not cancel: {text}"))),
                Err(e) => status.set(Some(format!("Could not cancel: {e}"))),
            }
        });
    };
    let copy_link = move |code: String| {
        let url = format!(
            "{}/m/{code}",
            window().location().origin().unwrap_or_default()
        );
        let _ = window().navigator().clipboard().write_text(&url);
        status.set(Some("Join link copied.".to_owned()));
    };

    view! {
        <main>
            <h1>"Your meetings"</h1>
            {move || {
                status
                    .get()
                    .map(|s| view! { <p data-testid="dashboard-status" role="status">{s}</p> })
            }}

            {move || if signed_out.get() {
                view! {
                    <p data-testid="signed-out">
                        "You're not signed in. "
                        <a href="/login">"Get a sign-in link"</a>
                    </p>
                }
                    .into_any()
            } else {
                view! {
                    <section>
                        <h2>"Schedule a meeting"</h2>
                        <form on:submit=create>
                            <label>
                                "Title"
                                <input
                                    type="text"
                                    placeholder="e.g. Weekly sync"
                                    data-testid="create-title"
                                    prop:value=title
                                    on:input=move |ev| title.set(event_target_value(&ev))
                                />
                            </label>
                            <label>
                                "Starts"
                                <input
                                    type="datetime-local"
                                    data-testid="create-starts"
                                    prop:value=starts
                                    on:input=move |ev| starts.set(event_target_value(&ev))
                                />
                            </label>
                            <label>
                                "Ends"
                                <input
                                    type="datetime-local"
                                    data-testid="create-ends"
                                    prop:value=ends
                                    on:input=move |ev| ends.set(event_target_value(&ev))
                                />
                            </label>
                            <button type="submit" data-testid="create-btn">"Create meeting"</button>
                        </form>
                    </section>

                    <section>
                        <h2>
                            "Upcoming"
                            {move || {
                                organizer_email
                                    .get()
                                    .map(|email| format!(" — signed in as {email}"))
                            }}
                        </h2>
                        <ul data-testid="mine-list">
                            <For each={move || meetings.get()} key=|m| m.meeting.id let:m>
                                {move || {
                                    // Keyed-`For` children are not re-rendered
                                    // when the key stays the same, so re-read
                                    // the signal and find the live item (same
                                    // pattern as the in-call roster).
                                    let current = meetings
                                        .get()
                                        .into_iter()
                                        .find(|q| q.meeting.id == m.meeting.id)
                                        .unwrap_or_else(|| m.clone());
                                    let code = current.meeting.code.clone();
                                    let starts = current
                                        .meeting
                                        .starts_at
                                        .format(&Rfc3339)
                                        .unwrap_or_default();
                                    let ends = current
                                        .meeting
                                        .ends_at
                                        .format(&Rfc3339)
                                        .unwrap_or_default();
                                    let state = match &current.joinability {
                                        Joinability::Open => "open".to_owned(),
                                        Joinability::NotYet { .. } => "not yet".to_owned(),
                                        Joinability::Ended => "ended".to_owned(),
                                        Joinability::Cancelled => "cancelled".to_owned(),
                                    };
                                    // Each handler owns its own clone of the
                                    // code: the row closure can be re-run, so
                                    // nothing may be captured by borrow.
                                    let copy_click = {
                                        let code = code.clone();
                                        move |_| copy_link(code.clone())
                                    };
                                    let cancel_testid = format!("cancel-{code}");
                                    let cancel_click = {
                                        let code = code.clone();
                                        move |_| cancel(code.clone())
                                    };
                                    let row_testid = format!("meeting-row-{code}");
                                    let summary = format!("{code} · {starts} → {ends} · {state}");
                                    let join_href = format!("/m/{code}");
                                    let ics_href = format!("/api/meetings/{code}/ics");
                                    let ics_testid = format!("ics-{code}");
                                    let copy_testid = format!("copy-link-{code}");
                                    let cancelled = current.meeting.cancelled;
                                    view! {
                                        <li data-testid=row_testid>
                                            <strong>{current.meeting.title.clone()}</strong>
                                            <small>{summary}</small>
                                            <a href=join_href>"Join"</a>
                                            <a href=ics_href download data-testid=ics_testid>
                                                ".ics"
                                            </a>
                                            <button data-testid=copy_testid on:click=copy_click>
                                                "Copy link"
                                            </button>
                                            {(!cancelled).then(move || {
                                                view! {
                                                    <button
                                                        data-testid=cancel_testid
                                                        on:click=cancel_click
                                                    >
                                                        "Cancel"
                                                    </button>
                                                }
                                            })}
                                        </li>
                                    }
                                }}
                            </For>
                        </ul>
                    </section>
                }
                    .into_any()
            }}
        </main>
    }
}
