//! The meeting flow owner and its views: prejoin → joining → in-call →
//! left / connection-lost. The state machine lives in `crate::flow`; this
//! module is its runtime — one spawned session task per join attempt —
//! and the visual surfaces for each phase.

use crate::api;
use crate::flow::{FlowPhase, JoinError, LeaveKind};
use crate::media;
use crate::prejoin::{Prejoin, PrejoinState};
use crate::rtc::{BrowserPeerConnection, BrowserSignalingStream, RtcClient};
use futures::channel::mpsc::UnboundedSender;
use futures::future::FutureExt;
use futures::stream::StreamExt;
use huddle_protocol::{ChatMessage, MeetingDto, Participant, ServerMessage, VideoLayer};
use leptos::oco::Oco;
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashSet;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use web_sys::MediaStream;

/// What the in-call controls ask the session task to do.
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
    /// Focus a participant's camera tile (`Some`) or clear the focus
    /// (`None`). Drives the simulcast layer policy: the focused origin
    /// upgrades to the high layer, everyone else stays low.
    Focus {
        origin: Option<Uuid>,
    },
    Leave,
}

/// Which direction the share-screen flow goes.
#[derive(Clone, Copy)]
enum ScreenCommand {
    Start,
    Stop,
}

/// The `FlowPhase` owner for one meeting URL. Renders the view for the
/// current phase and runs one session task per join attempt.
#[component]
pub fn MeetingFlow(code: String) -> impl IntoView {
    let phase = RwSignal::new(FlowPhase::Prejoin);
    let prejoin = PrejoinState::new();
    // The meeting code travels through a signal: every closure below
    // (join handler, phase view, end view) can then be `Copy`, so the
    // `on_join` callback stays `Copy + Send` as prejoin requires.
    let code = RwSignal::new(code);

    let roster = RwSignal::<Vec<Participant>>::new(Vec::new());
    let remote_streams = RwSignal::<Vec<(String, MediaStream)>>::new(Vec::new());
    let speaking = RwSignal::<HashSet<Uuid>>::new(Default::default());
    let chat = RwSignal::<Vec<ChatMessage>>::new(Vec::new());
    let chat_draft = RwSignal::new(String::new());
    let mic_on = RwSignal::new(true);
    let cam_on = RwSignal::new(true);
    let sharing = RwSignal::new(false);
    // The camera tile currently focused (if any): the focused origin
    // receives the high simulcast layer, everyone else the low one.
    let focused = RwSignal::<Option<Uuid>>::new(None);
    // Transient in-call notices (e.g. a denied screen-share picker).
    let status = RwSignal::<Option<String>>::new(None);
    // The live session's action inbox, present only between a
    // successful join and the end of the call.
    let ui_tx_slot = RwSignal::<Option<UnboundedSender<UiAction>>>::new(None);

    let on_join = {
        let signals = SessionSignals {
            phase,
            prejoin,
            roster,
            remote_streams,
            speaking,
            chat,
            mic_on,
            cam_on,
            sharing,
            status,
            ui_tx_slot,
        };
        move |name: String, local: Option<MediaStream>| {
            // One session at a time: buttons double-firing during the
            // Joining phase must not stack tasks.
            if signals.phase.get_untracked() != FlowPhase::Prejoin {
                return;
            }
            signals.prejoin.join_error.set(None);
            signals.phase.set(FlowPhase::Joining);
            let code = code.get_untracked();
            spawn_local(run_session(code, name, local, signals));
        }
    };

    view! {
        {move || match phase.get() {
            FlowPhase::Prejoin => {
                view! { <Prejoin code=code.get() state=prejoin on_join=on_join /> }
                    .into_any()
            }
            FlowPhase::Joining => view! { <JoiningView /> }.into_any(),
            FlowPhase::InCall => {
                view! {
                    <CallView
                        prejoin=prejoin
                        roster=roster
                        remote_streams=remote_streams
                        speaking=speaking
                        chat=chat
                        chat_draft=chat_draft
                        mic_on=mic_on
                        cam_on=cam_on
                        sharing=sharing
                        focused=focused
                        status=status
                        ui_tx_slot=ui_tx_slot
                    />
                }
                    .into_any()
            }
            FlowPhase::Ended(kind) => {
                view! { <EndView kind=kind code=code.get() meeting=prejoin.meeting /> }
                    .into_any()
            }
        }}
    }
}

/// Everything the session task needs to talk to the UI. Signals are
/// `Copy`, so the whole struct is too.
#[derive(Clone, Copy)]
struct SessionSignals {
    phase: RwSignal<FlowPhase>,
    prejoin: PrejoinState,
    roster: RwSignal<Vec<Participant>>,
    remote_streams: RwSignal<Vec<(String, MediaStream)>>,
    speaking: RwSignal<HashSet<Uuid>>,
    chat: RwSignal<Vec<ChatMessage>>,
    mic_on: RwSignal<bool>,
    cam_on: RwSignal<bool>,
    sharing: RwSignal<bool>,
    status: RwSignal<Option<String>>,
    ui_tx_slot: RwSignal<Option<UnboundedSender<UiAction>>>,
}

impl SessionSignals {
    /// End the flow because the connection dropped mid-session.
    fn connection_lost(self, reason: String) {
        self.ui_tx_slot.set(None);
        self.phase
            .set(FlowPhase::Ended(LeaveKind::ConnectionLost(reason)));
    }

    /// The locally formatted opening time, when the prejoin fetch knew
    /// it — it makes a "not open yet" refusal actionable.
    fn opens_at_hint(&self) -> Option<String> {
        match self.prejoin.joinability.get_untracked() {
            Some(huddle_protocol::Joinability::NotYet { opens_at }) => opens_at
                .format(&Rfc3339)
                .ok()
                .map(|raw| api::rfc3339_to_local(&raw)),
            _ => None,
        }
    }
}

/// One join attempt: connect, join, publish, then pump server messages
/// and UI actions until the call ends. Every failure path lands in a
/// distinct phase — see `docs/user-flow.md`.
async fn run_session(
    code: String,
    name: String,
    local: Option<MediaStream>,
    signals: SessionSignals,
) {
    let host = window()
        .location()
        .host()
        .unwrap_or_else(|_| "localhost".to_owned());
    let signaling = match BrowserSignalingStream::connect(&format!("ws://{host}/ws")).await {
        Ok(s) => s,
        Err(e) => {
            // The transport itself failed before any join message; the
            // copy in JoinError::Unreachable says what to do about it.
            leptos::logging::warn!("signaling connect failed: {e}");
            signals.prejoin.join_error.set(Some(JoinError::Unreachable));
            signals.phase.set(FlowPhase::Prejoin);
            return;
        }
    };
    // Test knob: `?turn=relay` forces every candidate through the TURN
    // relay (`iceTransportPolicy: "relay"`). The relay servers themselves
    // arrive with the Welcome and are applied inside `RtcClient::join`.
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
            signals
                .prejoin
                .join_error
                .set(Some(JoinError::PeerSetup(e.to_string())));
            signals.phase.set(FlowPhase::Prejoin);
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

    // The SFU labels each forwarded stream with the origin participant's
    // id as the msid. Our own publish is answered recvonly, but Chrome
    // still creates receivers for it — skip that self-echo so it does
    // not render as a phantom "Guest" tile. The stream id is captured
    // ONCE: Chrome's `MediaStream.id()` is unstable across reads.
    let local_id = local.as_ref().map(|s| s.id());
    {
        let streams = signals.remote_streams;
        peer.set_on_remote_stream(move |id, stream| {
            if local_id.as_deref() == Some(id) {
                return;
            }
            streams.update(|list| list.push((id.to_owned(), stream)));
        });
    }
    if let Some(local) = &local {
        peer.attach_stream(local).await;
    }

    let mut client = RtcClient::new(signaling, peer);
    if let Err(e) = client.join(&code, &name).await {
        signals
            .prejoin
            .join_error
            .set(Some(JoinError::from_join_failure(
                &e,
                signals.opens_at_hint(),
            )));
        signals.phase.set(FlowPhase::Prejoin);
        return;
    }
    signals.roster.set(client.roster().to_vec());
    if let Some(meeting) = client.meeting() {
        signals.prejoin.meeting.set(Some(meeting.clone()));
    }
    if let Err(e) = client.publish().await {
        signals.connection_lost(format!("Your audio and video could not be published ({e})"));
        return;
    }

    // The prejoin toggles survive the handover: the local stream is the
    // same object, so each track's `enabled` state is the truth to seed
    // the buttons from — muted on prejoin renders "Mic off" in-call.
    if let Some(local) = &local {
        signals
            .mic_on
            .set(media::track_kind_enabled(local, "audio"));
        signals
            .cam_on
            .set(media::track_kind_enabled(local, "video"));
    }

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
            signals.connection_lost(e.to_string());
            return;
        }
    }

    // The session is live: the controls can reach it, and the flow can
    // move to the in-call view.
    let (ui_tx, mut ui_rx) = futures::channel::mpsc::unbounded::<UiAction>();
    signals.ui_tx_slot.set(Some(ui_tx.clone()));
    signals.status.set(None);
    signals.phase.set(FlowPhase::InCall);

    // The screen-share flow reports the browser's own stop-sharing
    // signal (track ended) back into the session through the same
    // channel the buttons use; clone the sender before the speaking
    // watcher below moves it into its closure.
    let screen_ui_tx = ui_tx.clone();

    // Best-effort speaking detection: watch our own level and report
    // flips through the session. Failure is non-fatal — the call just
    // runs without the indicator.
    let _speaking_watch = match local.as_ref() {
        Some(stream) => match media::watch_speaking(stream, move |speaking| {
            let _ = ui_tx.unbounded_send(UiAction::Speaking(speaking));
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
                        signals.connection_lost(e.to_string());
                        break;
                    }
                    // A departed participant's tiles go with them: the
                    // camera stream carries their id, the screen stream
                    // carries `{id}-screen`.
                    if let ServerMessage::ParticipantLeft { id } = &msg {
                        let gone = id.to_string();
                        let screen = format!("{gone}-screen");
                        signals.remote_streams.update(|list| {
                            list.retain(|(stream_id, _)| stream_id != &gone && stream_id != &screen)
                        });
                    }
                    // A stopped share withdraws the owner's screen tile.
                    if let ServerMessage::Sharing { id, sharing: false } = &msg {
                        let screen = format!("{id}-screen");
                        signals
                            .remote_streams
                            .update(|list| list.retain(|(stream_id, _)| stream_id != &screen));
                    }
                    signals.roster.set(client.roster().to_vec());
                    signals.speaking.set(client.speaking().clone());
                    signals.chat.set(client.chat_log().to_vec());
                }
                Err(e) => {
                    signals.connection_lost(e.to_string());
                    break;
                }
            },
            action = ui_rx.select_next_some() => {
                match action {
                    UiAction::Mute { audio_muted, video_muted } => {
                        if let Err(e) = client.set_mute_state(audio_muted, video_muted).await {
                            signals.connection_lost(e.to_string());
                            break;
                        }
                    }
                    UiAction::Speaking(speaking) => {
                        if let Err(e) = client.set_speaking(speaking).await {
                            signals.connection_lost(e.to_string());
                            break;
                        }
                    }
                    UiAction::ShareScreen(command) => match command {
                        ScreenCommand::Start => {
                            // Acquire the capture first: a denied picker
                            // is a UI hiccup, not a call ender.
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
                                        Ok(()) => signals.sharing.set(true),
                                        Err(e) => {
                                            screen_end_guard.take();
                                            signals.sharing.set(false);
                                            signals.connection_lost(
                                                format!("Screen share could not start ({e})"),
                                            );
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    signals.status.set(Some(format!(
                                        "Screen share is unavailable: {e}"
                                    )));
                                }
                            }
                        }
                        ScreenCommand::Stop => {
                            // Dropping the watcher detaches `onended`; the
                            // task keeps no share state of its own.
                            screen_end_guard.take();
                            signals.sharing.set(false);
                            if let Err(e) = client.stop_screen_share().await {
                                signals.connection_lost(e.to_string());
                                break;
                            }
                        }
                    },
                    UiAction::Chat { body } => {
                        if let Err(e) = client.send_chat(body).await {
                            signals.connection_lost(e.to_string());
                            break;
                        }
                    }
                    UiAction::Focus { origin } => {
                        // The simulcast layer policy: the focused origin
                        // gets the high layer, every other camera (and
                        // any previously focused tile) drops to low.
                        // Screens and audio ignore the message.
                        let you = client.you().map(|p| p.id);
                        let preferences: Vec<(Participant, VideoLayer)> = client
                            .roster()
                            .iter()
                            .filter(|p| Some(p.id) != you)
                            .map(|p| {
                                let layer = if origin == Some(p.id) {
                                    VideoLayer::High
                                } else {
                                    VideoLayer::Low
                                };
                                (p.clone(), layer)
                            })
                            .collect();
                        let mut failure = None;
                        for (p, layer) in preferences {
                            if let Err(e) = client.set_layer_preference(p.id, layer).await {
                                failure = Some(e);
                                break;
                            }
                        }
                        if let Some(e) = failure {
                            signals.connection_lost(e.to_string());
                            break;
                        }
                    }
                    UiAction::Leave => {
                        let _ = client.leave().await;
                        signals.ui_tx_slot.set(None);
                        signals.phase.set(FlowPhase::Ended(LeaveKind::YouLeft));
                        break;
                    }
                }
            }
        }
    }
}

/// The join handshake's loading page: prejoin is gone, the call is not
/// yet up — say so instead of leaving a dead form on screen.
#[component]
fn JoiningView() -> impl IntoView {
    view! {
        <main class="page center">
            <div class="card center-card">
                <span class="spinner large" aria-hidden="true"></span>
                <h1 class="page-title">"Joining the meeting…"</h1>
                <p class="muted">"Setting up your audio and video. This takes a moment."</p>
            </div>
        </main>
    }
}

/// One media tile: a `<video>` bound to `stream` with a caption. Camera
/// tiles are clickable to focus (which drives the simulcast layer
/// policy); the local and screen tiles are not.
#[component]
fn VideoTile(
    stream: MediaStream,
    label: Oco<'static, str>,
    muted: bool,
    testid: Oco<'static, str>,
    speaking: bool,
    screen: bool,
    focused: bool,
    on_click: Option<Callback<()>>,
) -> impl IntoView {
    let video_ref = NodeRef::<leptos::html::Video>::new();
    Effect::new(move |_| {
        if let Some(video) = video_ref.get() {
            video.set_muted(muted);
            video.set_src_object(Some(&stream));
            crate::play_silently(&video);
        }
    });
    let mut class = match (speaking, screen) {
        (_, true) => "tile screen".to_owned(),
        (true, false) => "tile speaking".to_owned(),
        (false, false) => "tile".to_owned(),
    };
    if focused {
        class.push_str(" focused");
    }
    view! {
        <figure
            class=class
            data-testid=testid
            on:click=move |_| if let Some(cb) = &on_click { cb.run(()) }
        >
            <video node_ref=video_ref autoplay muted playsinline></video>
            <figcaption>{label}</figcaption>
        </figure>
    }
}

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

/// The in-call surface: tiles, roster, chat, and controls. The session
/// task behind it owns the `RtcClient`; actions travel through
/// `ui_tx_slot`.
#[component]
fn CallView(
    prejoin: PrejoinState,
    roster: RwSignal<Vec<Participant>>,
    remote_streams: RwSignal<Vec<(String, MediaStream)>>,
    speaking: RwSignal<HashSet<Uuid>>,
    chat: RwSignal<Vec<ChatMessage>>,
    chat_draft: RwSignal<String>,
    mic_on: RwSignal<bool>,
    cam_on: RwSignal<bool>,
    sharing: RwSignal<bool>,
    focused: RwSignal<Option<Uuid>>,
    status: RwSignal<Option<String>>,
    ui_tx_slot: RwSignal<Option<UnboundedSender<UiAction>>>,
) -> impl IntoView {
    let local = prejoin.stream.get_untracked();
    let meeting_title = move || {
        prejoin
            .meeting
            .get()
            .map(|m| m.title)
            .unwrap_or_else(|| "Meeting".to_owned())
    };

    // Adaptive grid: square-root packing keeps tiles usable from a
    // one-on-one to a full room (CSS clamps it further on phones).
    let cols = Memo::new(move |_| match remote_streams.get().len() + 1 {
        1 => 1,
        2..=4 => 2,
        5..=9 => 3,
        _ => 4,
    });

    let send = move |action: UiAction| {
        if let Some(tx) = ui_tx_slot.get_untracked() {
            let _ = tx.unbounded_send(action);
        }
    };

    let toggle_mic = {
        let local = local.clone();
        move |_| {
            mic_on.update(|v| *v = !*v);
            let muted = !mic_on.get_untracked();
            if let Some(stream) = &local {
                media::set_track_kind_enabled(stream, "audio", !muted);
            }
            send(UiAction::Mute {
                audio_muted: Some(muted),
                video_muted: None,
            });
        }
    };
    let toggle_cam = {
        let local = local.clone();
        move |_| {
            cam_on.update(|v| *v = !*v);
            let muted = !cam_on.get_untracked();
            if let Some(stream) = &local {
                media::set_track_kind_enabled(stream, "video", !muted);
            }
            send(UiAction::Mute {
                audio_muted: None,
                video_muted: Some(muted),
            });
        }
    };
    let leave = { move |_| send(UiAction::Leave) };
    let share = {
        move |_| {
            let command = if sharing.get_untracked() {
                ScreenCommand::Stop
            } else {
                ScreenCommand::Start
            };
            send(UiAction::ShareScreen(command));
        }
    };

    // Send the chat draft, shared by the button and the Enter key.
    fn submit_chat_body(chat_draft: RwSignal<String>, send: &dyn Fn(UiAction)) {
        let body = chat_draft.get_untracked().trim().to_owned();
        if body.is_empty() {
            return;
        }
        chat_draft.set(String::new());
        send(UiAction::Chat { body });
    }
    let on_send_click = { move |_: web_sys::MouseEvent| submit_chat_body(chat_draft, &send) };
    let on_send_key = {
        move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "Enter" {
                submit_chat_body(chat_draft, &send);
            }
        }
    };

    let tiles_style = move || format!("--cols: {}", cols.get());

    // A departed (or unfocused-away) participant must not stay focused.
    Effect::new(move |_| {
        let roster = roster.get();
        if let Some(id) = focused.get_untracked()
            && !roster.iter().any(|p| p.id == id)
        {
            focused.set(None);
        }
    });

    view! {
        <div class="call">
            <header class="call-header">
                <a class="wordmark" href="/">"huddle"</a>
                <span class="on-air" title="You are live with everyone in the meeting">
                    <span class="on-air-dot" aria-hidden="true"></span>
                    "ON AIR"
                </span>
                <p class="call-title">{meeting_title}</p>
                {move || {
                    status
                        .get()
                        .map(|s| view! { <p data-testid="call-status" role="status">{s}</p> })
                }}
            </header>

            <div class="call-body">
                <section class="stage" aria-label="Video tiles">
                    <div class="tiles" style=tiles_style>
                        {local.clone().map(|stream| {
                            view! {
                                <VideoTile
                                    stream=stream
                                    label=Oco::from("You")
                                    muted=true
                                    testid=Oco::from("tile-you")
                                    speaking=false
                                    screen=false
                                    focused=false
                                    on_click=None
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
                                let id = Uuid::parse_str(origin).ok();
                                // The speaking set is keyed by participant id, so
                                // it tells us whose camera tile to ring.
                                let is_speaking = !is_screen
                                    && id.is_some_and(|id| speaking.get().contains(&id));
                                let is_focused =
                                    !is_screen && focused.get().is_some_and(|f| Some(f) == id);
                                // Clicking a camera tile focuses it (upgrading
                                // that origin to the high simulcast layer);
                                // clicking the focused tile again clears it.
                                let on_click = if is_screen {
                                    None
                                } else {
                                    Some(Callback::from(move || {
                                        let next = if focused.get_untracked() == id {
                                            None
                                        } else {
                                            id
                                        };
                                        focused.set(next);
                                        send(UiAction::Focus { origin: next });
                                    }))
                                };
                                let (label, testid) = if is_screen {
                                    (
                                        Oco::from(format!("{base}'s screen")),
                                        Oco::from(format!("tile-{base}-screen")),
                                    )
                                } else {
                                    (Oco::from(base.clone()), Oco::from(format!("tile-{base}")))
                                };
                                view! {
                                    <VideoTile
                                        stream=s.1.clone()
                                        label=label
                                        muted=false
                                        testid=testid
                                        speaking=is_speaking
                                        screen=is_screen
                                        focused=is_focused
                                        on_click=on_click
                                    />
                                }
                            }}
                        </For>
                    </div>
                </section>

                <aside class="side">
                    <section class="roster-block">
                        <h2>"In this meeting"</h2>
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
                    </section>

                    <section class="chat">
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
                        <div class="chat-composer">
                            <input
                                type="text"
                                placeholder="Message everyone"
                                data-testid="chat-input"
                                prop:value=chat_draft
                                on:input=move |ev| chat_draft.set(event_target_value(&ev))
                                on:keydown=on_send_key
                            />
                            <button class="btn" data-testid="chat-send" on:click=on_send_click>
                                "Send"
                            </button>
                        </div>
                    </section>
                </aside>
            </div>

            <footer class="call-controls">
                <button
                    class="btn toggle"
                    class:is-off=move || !mic_on.get()
                    data-testid="toggle-mic"
                    on:click=toggle_mic
                >
                    {move || if mic_on.get() { "Mic on" } else { "Mic off" }}
                </button>
                <button
                    class="btn toggle"
                    class:is-off=move || !cam_on.get()
                    data-testid="toggle-cam"
                    on:click=toggle_cam
                >
                    {move || if cam_on.get() { "Camera on" } else { "Camera off" }}
                </button>
                <button
                    class="btn toggle"
                    class:is-off=move || !sharing.get()
                    data-testid="share-screen"
                    on:click=share
                >
                    {move || if sharing.get() { "Stop sharing" } else { "Share screen" }}
                </button>
                <button class="btn danger" data-testid="leave-btn" on:click=leave>
                    "Leave"
                </button>
            </footer>
        </div>
    }
}

/// The shared skeleton for both ways a call ends. The copy and the
/// primary action differ — leaving is a choice, being dropped is not —
/// but both offer a way back in and a way out.
#[component]
fn EndView(kind: LeaveKind, code: String, meeting: RwSignal<Option<MeetingDto>>) -> impl IntoView {
    let reload = move |_| {
        let _ = window().location().reload();
    };

    // Leaving is a choice; being dropped is not. Same skeleton,
    // different words and primary action.
    let (heading, status_text, primary, primary_testid, disconnected) = match kind {
        LeaveKind::YouLeft => (
            "You left the meeting",
            "You left the meeting.".to_owned(),
            "Rejoin meeting",
            "rejoin-btn",
            false,
        ),
        LeaveKind::ConnectionLost(ref reason) => (
            "You've been disconnected",
            format!("Connection lost: {reason}"),
            "Reconnect",
            "reconnect-btn",
            true,
        ),
    };

    view! {
        <main class="page center">
            <header class="site-nav">
                <a class="wordmark" href="/">"huddle"</a>
            </header>
            <div class="card end-card" class:disconnected=disconnected>
                <h1 class="page-title">{heading}</h1>
                <p data-testid="call-status" role="status">{status_text}</p>

                {move || {
                    meeting.get().map(|m| {
                        let starts = m
                            .starts_at
                            .format(&Rfc3339)
                            .ok()
                            .map(|raw| api::rfc3339_to_local(&raw))
                            .unwrap_or_default();
                        let ends = m
                            .ends_at
                            .format(&Rfc3339)
                            .ok()
                            .map(|raw| api::rfc3339_to_local(&raw))
                            .unwrap_or_default();
                        view! {
                            <dl class="end-info">
                                <dt>"Meeting"</dt>
                                <dd>{m.title}</dd>
                                <dt>"Code"</dt>
                                <dd><code>{m.code}</code></dd>
                                <dt>"Scheduled"</dt>
                                <dd>{format!("{starts} to {ends}")}</dd>
                            </dl>
                        }
                    })
                }}

                <div class="end-actions">
                    <button class="btn primary" data-testid=primary_testid on:click=reload>
                        {primary}
                    </button>
                    <a class="btn" href="/meetings">"Back to my meetings"</a>
                </div>
                <p class="muted">"Meeting code: "{code}</p>
            </div>
        </main>
    }
}
