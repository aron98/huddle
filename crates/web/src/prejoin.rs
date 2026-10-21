//! The prejoin page (`/m/{code}`): meeting info, camera preview, name,
//! passcode (when the meeting has one), and device pickers. This is the
//! `Prejoin` state of the flow defined in `crate::flow`; the flow owner
//! (`crate::call::MeetingFlow`) hands the signals down so a failed join
//! can land back here with everything the user set up still intact.

use crate::api;
use crate::audio_settings;
use crate::flow::JoinError;
use crate::media;
use huddle_protocol::{Joinability, MeetingDto, MeetingResponse};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::cell::Cell;
use std::rc::Rc;
use time::format_description::well_known::Rfc3339;
use wasm_bindgen::JsCast;

/// The prejoin signals, owned by the flow (so a failed join can land
/// back here with the camera preview, name, and devices intact).
#[derive(Clone, Copy)]
pub struct PrejoinState {
    pub name: RwSignal<String>,
    pub stream: RwSignal<Option<web_sys::MediaStream>>,
    pub devices: RwSignal<Vec<media::Device>>,
    pub camera_id: RwSignal<Option<String>>,
    pub mic_id: RwSignal<Option<String>>,
    pub cam_on: RwSignal<bool>,
    pub mic_on: RwSignal<bool>,
    /// The per-meeting passcode, sent as the join token. Kept in the
    /// flow-owned state so a refused join lands back here with it
    /// intact, like the name.
    pub passcode: RwSignal<String>,
    /// Explicit AEC / noise-suppression constraints (issue #34),
    /// seeded from the per-browser localStorage prefs and re-applied
    /// on every acquisition. Owned here so the in-call controls —
    /// which re-acquire the mic live — read the same choice.
    pub audio: RwSignal<audio_settings::AudioSettings>,
    /// Camera/mic acquisition failure; `None` while media works.
    pub media_error: RwSignal<Option<String>>,
    /// Meeting info from `GET /api/meetings/{code}`, for the title and
    /// the "starts at" hint.
    pub meeting: RwSignal<Option<MeetingDto>>,
    /// The true title when this browser could decrypt the stored blob
    /// (the organizer's device, issue #29); `None` for guests, who see
    /// the masked label until the E2EE relay delivers the real one.
    pub decrypted_title: RwSignal<Option<String>>,
    /// Joinability from the same fetch; refreshed by a poll while the
    /// meeting has not opened yet.
    pub joinability: RwSignal<Option<Joinability>>,
    /// Set by the flow owner when a join attempt failed.
    pub join_error: RwSignal<Option<JoinError>>,
    /// The link's code matches no meeting (GET returned 404).
    pub missing: RwSignal<bool>,
    /// Background-blur state for the camera (see `effects.rs`).
    pub effects: crate::effects::Effects,
}

impl Default for PrejoinState {
    fn default() -> Self {
        Self::new()
    }
}

impl PrejoinState {
    pub fn new() -> Self {
        Self {
            name: RwSignal::new(String::new()),
            stream: RwSignal::new(None),
            devices: RwSignal::new(Vec::new()),
            camera_id: RwSignal::new(None),
            mic_id: RwSignal::new(None),
            cam_on: RwSignal::new(true),
            mic_on: RwSignal::new(true),
            passcode: RwSignal::new(String::new()),
            audio: RwSignal::new(audio_settings::load()),
            media_error: RwSignal::new(None),
            meeting: RwSignal::new(None),
            decrypted_title: RwSignal::new(None),
            joinability: RwSignal::new(None),
            join_error: RwSignal::new(None),
            missing: RwSignal::new(false),
            effects: crate::effects::Effects::new(),
        }
    }
}

/// (Re-)acquire the camera and mic with the currently picked devices.
/// Fresh getUserMedia tracks always come back enabled, so the toggle
/// state is re-applied: switching the mic must not resurrect a muted
/// track, and it must not disable a live camera either.
fn acquire(state: PrejoinState) {
    state.media_error.set(None);
    let camera_id = state.camera_id.get_untracked();
    let mic_id = state.mic_id.get_untracked();
    let audio = state.audio.get_untracked();
    spawn_local(async move {
        // Stop the current capture first: while a track is open the
        // browser keeps the per-device audio processing (AEC/NS) it
        // started with, and a concurrent getUserMedia inherits the
        // old settings instead of the requested ones (issue #34).
        // This also keeps switched-away devices released.
        if let Some(old) = state.stream.get_untracked() {
            for track in media::tracks_of(&old) {
                track.stop();
            }
            state.stream.set(None);
        }
        match media::request_media(camera_id.as_deref(), mic_id.as_deref(), audio).await {
            Ok(stream) => {
                media::set_track_kind_enabled(&stream, "audio", state.mic_on.get_untracked());
                media::set_track_kind_enabled(&stream, "video", state.cam_on.get_untracked());
                state.effects.attach_source(&stream);
                state.stream.set(Some(stream));
            }
            Err(e) => state.media_error.set(Some(e)),
        }
        // Labels only appear once permission is granted; list after the
        // grab. Listing failing is non-fatal: the pickers just stay empty.
        if let Ok(found) = media::list_devices().await {
            state.devices.set(found);
        }
    });
}

/// While a meeting has not opened yet, re-check its joinability
/// periodically so the hint (and the banner after a refused join) can
/// flip to "open" without a manual reload. The poller clears itself as
/// soon as the meeting is no longer "not yet", so at most one poller
/// outlives each prejoin mount.
fn poll_until_open(code: String, state: PrejoinState) {
    let handle: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
    let tick = {
        let handle = handle.clone();
        let code = code.clone();
        move || {
            // The future must be 'static, but it must not drain this
            // closure's environment: hand it fresh clones per tick.
            let code = code.clone();
            let handle = handle.clone();
            spawn_local(async move {
                if let Ok((200, body)) =
                    api::fetch_text("GET", &format!("/api/meetings/{code}"), None).await
                    && let Ok(response) = serde_json::from_str::<MeetingResponse>(&body)
                {
                    let still_not_open = matches!(response.joinability, Joinability::NotYet { .. });
                    state.joinability.set(Some(response.joinability));
                    apply_meeting_title(state, response.meeting).await;
                    if !still_not_open && let Some(id) = handle.get() {
                        window().clear_interval_with_handle(id);
                        handle.set(None);
                    }
                }
            });
        }
    };
    let callback = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(tick);
    if let Ok(id) = window().set_interval_with_callback_and_timeout_and_arguments_0(
        callback.as_ref().unchecked_ref::<js_sys::Function>(),
        15_000,
    ) {
        handle.set(Some(id));
        // JS holds the callback by reference, so the Rust closure must
        // outlive the interval; the poller is self-limiting, which bounds
        // this leak by the meeting opening.
        std::mem::forget(callback);
    }
}

/// Decrypt the stored title blob when this browser is the organizer's
/// (the title key is the organizer identity's self-ECDH) and publish
/// the meeting DTO with the true title; guests keep the masked label
/// the server sent (issue #29).
async fn apply_meeting_title(state: PrejoinState, mut meeting: MeetingDto) {
    if let Some(ciphertext) = meeting.title_ciphertext.clone()
        && let Ok(Some(title)) = crate::e2ee::titles::decrypt_title(&ciphertext).await
    {
        state.decrypted_title.set(Some(title.clone()));
        meeting.title = title;
    }
    state.meeting.set(Some(meeting));
}

/// The prejoin page. `on_join` hands (name, stream) to the flow owner,
/// which switches the flow into the Joining state.
#[component]
pub fn Prejoin(
    code: String,
    state: PrejoinState,
    on_join: impl Fn(String, Option<web_sys::MediaStream>) + Copy + Send + 'static,
) -> impl IntoView {
    let name_hint = RwSignal::<Option<String>>::new(None);

    // Grab the default camera/mic on mount. Re-entering prejoin after a
    // failed join skips this: the stream is still live.
    if state.stream.get_untracked().is_none() {
        acquire(state);
    }

    // Meeting info drives the title, the "starts at" hint, and the
    // dead-link notice. Failing to fetch it is not fatal — the WS join
    // is the authoritative check.
    {
        let code = code.clone();
        spawn_local(async move {
            match api::fetch_text("GET", &format!("/api/meetings/{code}"), None).await {
                Ok((200, body)) => {
                    if let Ok(response) = serde_json::from_str::<MeetingResponse>(&body) {
                        let not_yet = matches!(response.joinability, Joinability::NotYet { .. });
                        state.joinability.set(Some(response.joinability));
                        apply_meeting_title(state, response.meeting).await;
                        if not_yet {
                            poll_until_open(code, state);
                        }
                    }
                }
                Ok((404, _)) => state.missing.set(true),
                _ => {}
            }
        });
    }

    // Show the acquired stream in the preview element. The outgoing
    // stream (blurred video while blur is on) is preferred once it
    // exists; its in-place track swaps propagate automatically.
    Effect::new(move |_| {
        let Some(stream) = state
            .effects
            .outgoing_tracked()
            .or_else(|| state.stream.get())
        else {
            return;
        };
        let Some(element) = document().get_element_by_id("preview") else {
            return;
        };
        let Ok(video) = element.dyn_into::<web_sys::HtmlVideoElement>() else {
            return;
        };
        video.set_muted(true);
        video.set_src_object(Some(&stream));
        crate::play_silently(&video);
    });

    let on_camera_change = move |ev| {
        let id = event_target_value(&ev);
        state
            .camera_id
            .set(if id.is_empty() { None } else { Some(id) });
        acquire(state);
    };
    let on_mic_change = move |ev| {
        let id = event_target_value(&ev);
        state
            .mic_id
            .set(if id.is_empty() { None } else { Some(id) });
        acquire(state);
    };

    // Each toggle touches only its own track kind — a mic-off must never
    // black out the camera preview (issue #7).
    let toggle_cam = move |_| {
        state.cam_on.update(|v| *v = !*v);
        if let Some(stream) = state.stream.get_untracked() {
            media::set_track_kind_enabled(&stream, "video", state.cam_on.get_untracked());
        }
    };
    let toggle_mic = move |_| {
        state.mic_on.update(|v| *v = !*v);
        if let Some(stream) = state.stream.get_untracked() {
            media::set_track_kind_enabled(&stream, "audio", state.mic_on.get_untracked());
        }
    };
    // Blur is async (model load on first toggle); a failed toggle just
    // surfaces the notice instead of unwinding the page.
    let toggle_blur = move |_| {
        let effects = state.effects;
        let target = !effects.blur_on.get_untracked();
        spawn_local(async move {
            let _ = effects.set_blur(target).await;
        });
    };

    let cameras = Memo::new(move |_| {
        state
            .devices
            .get()
            .into_iter()
            .filter(|d| d.kind == media::DeviceKind::Camera)
            .collect::<Vec<_>>()
    });
    let microphones = Memo::new(move |_| {
        state
            .devices
            .get()
            .into_iter()
            .filter(|d| d.kind == media::DeviceKind::Microphone)
            .collect::<Vec<_>>()
    });

    let on_name_input = move |ev| {
        state.name.set(event_target_value(&ev));
        name_hint.set(None);
        // Editing the name clears a "taken" rejection: the fix is exactly
        // this field.
        if matches!(state.join_error.get_untracked(), Some(JoinError::NameTaken)) {
            state.join_error.set(None);
        }
    };

    let on_passcode_input = move |ev| {
        state.passcode.set(event_target_value(&ev));
        // Typing here is the fix for a passcode refusal — drop it so the
        // inline error (and any banner) disappears as soon as the user
        // reacts, mirroring the name field.
        if matches!(
            state.join_error.get_untracked(),
            Some(JoinError::PasscodeRequired)
        ) {
            state.join_error.set(None);
        }
    };

    // The passcode field appears when the meeting is known to have one
    // (proactive, from the prejoin fetch) or when the server just said a
    // passcode is required (meeting info may not have arrived).
    let passcode_needed = move || {
        state.meeting.get().is_some_and(|m| m.has_passcode)
            || matches!(state.join_error.get(), Some(JoinError::PasscodeRequired))
    };
    // A passcode refusal is an inline field error, like a taken name.
    let inline_passcode_error = move || {
        if let Some(JoinError::PasscodeRequired) = state.join_error.get() {
            return Some(JoinError::PasscodeRequired.detail(&state.name.get_untracked()));
        }
        None
    };

    let retry_join = {
        move || {
            let name = state.name.get_untracked().trim().to_owned();
            let stream = state.stream.get_untracked();
            state.join_error.set(None);
            on_join(name, stream);
        }
    };

    let submit = {
        move |ev: leptos::ev::SubmitEvent| {
            ev.prevent_default();
            if state.name.get_untracked().trim().is_empty() {
                name_hint.set(Some("Enter your name to join.".to_owned()));
                return;
            }
            retry_join();
        }
    };

    let retry_media = move |_| acquire(state);

    let heading = move || {
        if state.missing.get() {
            "This link doesn't match a meeting".to_owned()
        } else {
            state
                .meeting
                .get()
                .map(|m| m.title)
                .unwrap_or_else(|| "Join meeting".to_owned())
        }
    };

    // One honest line about the meeting's schedule, where known.
    let schedule_hint = move || match state.joinability.get() {
        Some(Joinability::Open) => None,
        Some(Joinability::NotYet { opens_at }) => {
            let when = opens_at
                .format(&Rfc3339)
                .ok()
                .map(|raw| api::rfc3339_to_local(&raw))
                .unwrap_or_default();
            Some(format!("Starts {when}"))
        }
        Some(Joinability::Ended) => Some("This meeting has ended".to_owned()),
        Some(Joinability::Cancelled) => Some("This meeting was cancelled".to_owned()),
        None => None,
    };

    // A taken name is an inline field error; everything else is a banner
    // with a retry.
    let inline_name_error = move || {
        if let Some(JoinError::NameTaken) = state.join_error.get() {
            return Some(JoinError::NameTaken.detail(&state.name.get_untracked()));
        }
        name_hint.get()
    };
    let banner = move || {
        state.join_error.get().and_then(|err| {
            // NameTaken and PasscodeRequired render as inline field
            // errors instead of a banner: the fix is the field itself.
            (!matches!(err, JoinError::NameTaken | JoinError::PasscodeRequired)).then(move || {
                let headline = err.headline();
                let detail = err.detail(&state.name.get_untracked());
                view! {
                    <div class="banner" role="alert">
                        <p class="banner-title">{headline}</p>
                        <p class="banner-detail">{detail}</p>
                        <div class="banner-actions">
                            <button
                                class="btn primary"
                                data-testid="retry-join"
                                on:click=move |_| retry_join()
                            >
                                "Try joining again"
                            </button>
                        </div>
                    </div>
                }
            })
        })
    };

    view! {
        <main class="page">
            <header class="site-nav">
                <a class="wordmark" href="/">"huddle"</a>
            </header>

            <section class="prejoin">
                <div class="prejoin-media">
                    <h1 class="page-title" data-testid="prejoin-title">{heading}</h1>
                    <p class="schedule-hint">
                        {move || schedule_hint().map(|hint| view! { <span class="chip">{hint}</span> })}
                        {move || {
                            state
                                .meeting
                                .get()
                                .is_some_and(|m| m.e2ee)
                                .then(|| {
                                    view! {
                                        <span class="chip chip-e2ee" data-testid="prejoin-e2ee">
                                            "End-to-end encrypted"
                                        </span>
                                    }
                                })
                        }}
                        "Meeting code "
                        <code>{code.clone()}</code>
                    </p>
                    <div class="preview-frame">
                        <video id="preview" autoplay muted playsinline></video>
                        {move || match (state.stream.get(), state.media_error.get()) {
                            (None, Some(err)) => view! {
                                <div class="preview-overlay">
                                    <p class="overlay-title">"Camera and mic are unavailable"</p>
                                    <p class="overlay-detail">{err}</p>
                                    <button class="btn" on:click=retry_media>"Try again"</button>
                                </div>
                            }
                                .into_any(),
                            (None, None) => view! {
                                <div class="preview-overlay">
                                    <span class="spinner" aria-hidden="true"></span>
                                    <p class="overlay-title">"Getting your camera and mic ready…"</p>
                                    <p class="overlay-detail">"Your browser may ask for permission."</p>
                                </div>
                            }
                                .into_any(),
                            _ => ().into_any(),
                        }}
                    </div>
                    <div class="prejoin-toggles">
                        <button
                            class="btn toggle"
                            class:is-off=move || !state.cam_on.get()
                            data-testid="prejoin-toggle-cam"
                            on:click=toggle_cam
                        >
                            {move || if state.cam_on.get() { "Camera on" } else { "Camera off" }}
                        </button>
                        <button
                            class="btn toggle"
                            class:is-off=move || !state.mic_on.get()
                            data-testid="prejoin-toggle-mic"
                            on:click=toggle_mic
                        >
                            {move || if state.mic_on.get() { "Mic on" } else { "Mic off" }}
                        </button>
                        <button
                            class="btn toggle"
                            class:is-off=move || !state.effects.blur_on.get()
                            data-testid="prejoin-toggle-blur"
                            on:click=toggle_blur
                        >
                            {move || if state.effects.blur_on.get() { "Blur on" } else { "Blur off" }}
                        </button>
                    </div>
                    {move || {
                        state
                            .effects
                            .notice
                            .get()
                            .map(|n| {
                                view! {
                                    <p class="field-error" data-testid="effects-notice" role="status">
                                        {n}
                                    </p>
                                }
                            })
                    }}
                </div>

                <form class="prejoin-panel" on:submit=submit>
                    <label class="field">
                        "Your name"
                        <input
                            type="text"
                            placeholder="e.g. Alice"
                            data-testid="name-input"
                            prop:value=state.name
                            on:input=on_name_input
                        />
                    </label>
                    {move || {
                        inline_name_error()
                            .map(|message| view! { <p class="field-error" role="alert">{message}</p> })
                    }}

                    {move || {
                        passcode_needed().then(|| {
                            view! {
                                <label class="field">
                                    "Passcode"
                                    <input
                                        type="password"
                                        placeholder="Meeting passcode"
                                        autocomplete="off"
                                        data-testid="passcode-input"
                                        prop:value=state.passcode
                                        on:input=on_passcode_input
                                    />
                                </label>
                                {move || {
                                    inline_passcode_error().map(
                                        |message| {
                                            view! {
                                                <p class="field-error" role="alert">{message}</p>
                                            }
                                        },
                                    )
                                }}
                            }
                        })
                    }}

                    <label class="field">
                        "Camera"
                        <select data-testid="camera-select" on:change=on_camera_change>
                            <option value="">"Default"</option>
                            <For each={move || cameras.get()} key=|d| d.id.clone() let:d>
                                <option
                                    value={d.id.clone()}
                                    selected={move || state.camera_id.get().as_deref() == Some(d.id.as_str())}
                                >
                                    {d.label.clone()}
                                </option>
                            </For>
                        </select>
                    </label>

                    <label class="field">
                        "Microphone"
                        <select data-testid="mic-select" on:change=on_mic_change>
                            <option value="">"Default"</option>
                            <For each={move || microphones.get()} key=|d| d.id.clone() let:d>
                                <option
                                    value={d.id.clone()}
                                    selected={move || state.mic_id.get().as_deref() == Some(d.id.as_str())}
                                >
                                    {d.label.clone()}
                                </option>
                            </For>
                        </select>
                    </label>

                    // Echo suppression is strictly client-side (issue
                    // #34): explicit constraints on every acquisition,
                    // user toggles, per-browser persistence. A flip
                    // re-acquires the mic right here, so the user can
                    // hear the difference before joining.
                    <audio_settings::AudioSettingsControls
                        settings=state.audio
                        on_change=Callback::new(move |_: audio_settings::AudioSettings| acquire(state))
                    />
                    <p class="audio-hint">"Hearing echo on speakers? A headset is the most reliable fix."</p>

                    // A call is media-first: until getUserMedia has
                    // produced a local stream there is nothing to
                    // publish, and an offer with zero m-lines would be
                    // rejected by the SFU.
                    <button
                        type="submit"
                        class="btn primary join"
                        data-testid="join-btn"
                        disabled=move || state.stream.get().is_none() || state.missing.get()
                    >
                        {move || if state.stream.get().is_some() {
                            "Join meeting"
                        } else {
                            "Getting camera and mic…"
                        }}
                    </button>
                </form>
            </section>

            {move || {
                if state.missing.get() {
                    view! {
                        <div class="banner" role="alert">
                            <p class="banner-title">"This link doesn't match a meeting"</p>
                            <p class="banner-detail">
                                "The code in this link doesn't belong to any meeting. \
                                 Ask the organizer to send a fresh link."
                            </p>
                        </div>
                    }
                        .into_any()
                } else {
                    banner().map(|b| b.into_any()).unwrap_or_else(|| ().into_any())
                }
            }}
        </main>
    }
}
