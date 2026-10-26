#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![allow(async_fn_in_trait)] // our boundary traits are used generically, never dyn/Send

//! The huddle web client.
//!
//! Leptos CSR app. The user flow is an explicit state machine — see
//! `flow.rs` and `docs/user-flow.md`:
//!
//! - `landing`/`login`/`dashboard` pages live here;
//! - `/m/{code}` is `call::MeetingFlow`, which owns the phases
//!   (prejoin → joining → in-call → left/connection-lost).

#[cfg(target_arch = "wasm32")]
pub mod api;
#[cfg(target_arch = "wasm32")]
pub mod audio_settings;
#[cfg(target_arch = "wasm32")]
pub mod call;
pub mod e2ee;
#[cfg(target_arch = "wasm32")]
pub mod effects;
pub mod flow;
#[cfg(target_arch = "wasm32")]
pub mod media;
#[cfg(target_arch = "wasm32")]
pub mod prejoin;
pub mod rtc;

use leptos::prelude::*;
#[cfg(target_arch = "wasm32")]
use leptos::task::spawn_local;
#[cfg(target_arch = "wasm32")]
use uuid::Uuid;

/// Consume a media-play promise so a late rejection (e.g. the tile was
/// unmounted mid-play) never surfaces as an unhandled page error.
#[cfg(target_arch = "wasm32")]
pub(crate) fn play_silently(video: &web_sys::HtmlVideoElement) {
    if let Ok(promise) = video.play() {
        spawn_local(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        });
    }
}

/// Application root. The URL decides the page:
/// `/` → landing, `/m/{code}` → the meeting flow, `/login` → sign-in,
/// `/meetings` → the organizer dashboard.
#[component]
pub fn App() -> impl IntoView {
    let path = window().location().pathname().unwrap_or_default();
    let code = path
        .strip_prefix("/m/")
        .map(|c| c.trim_end_matches('/'))
        .filter(|c| !c.is_empty());

    match code {
        Some(code) => {
            #[cfg(target_arch = "wasm32")]
            {
                view! { <call::MeetingFlow code={code.to_owned()} /> }.into_any()
            }
            // On native (test) builds the meeting page stays a
            // placeholder: the browser glue is wasm-only, and the
            // signaling driver is tested via the mock server.
            #[cfg(not(target_arch = "wasm32"))]
            {
                view! { <MeetingFlow code={code.to_owned()} /> }.into_any()
            }
        }
        None => match path.as_str() {
            #[cfg(target_arch = "wasm32")]
            "/login" => view! { <Login /> }.into_any(),
            #[cfg(target_arch = "wasm32")]
            "/meetings" => view! { <Dashboard /> }.into_any(),
            _ => view! { <Landing /> }.into_any(),
        },
    }
}

// Native placeholder for the meeting flow (see `App`).
#[cfg(not(target_arch = "wasm32"))]
#[component]
fn MeetingFlow(code: String) -> impl IntoView {
    view! {
        <main>
            <h1>"Join meeting"</h1>
            <p>{format!("Meeting code: {code}")}</p>
        </main>
    }
}

/// The front page: what huddle is, and the two ways in — open a meeting
/// link, or sign in to schedule one.
#[component]
fn Landing() -> impl IntoView {
    let code = RwSignal::new(String::new());

    let open_meeting = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let code = code.get_untracked().trim().to_owned();
        if code.is_empty() {
            return;
        }
        let _ = window().location().set_href(&format!("/m/{code}"));
    };

    view! {
        <main class="page landing">
            <header class="site-nav">
                <a class="wordmark" href="/">
                    <span class="on-air-dot" aria-hidden="true"></span>
                    "huddle"
                </a>
                <nav class="site-links">
                    <a href="/login" data-testid="nav-login">"Sign in"</a>
                    <a href="/meetings" data-testid="nav-meetings">"My meetings"</a>
                </nav>
            </header>

            <section class="hero">
                <h1 class="hero-title">"Get everyone into one room."</h1>
                <p class="hero-lede">
                    "Self-hosted video meetings. You run the server; your guests \
                     just open a link. No guest accounts, no third parties on the call."
                </p>
                <div class="hero-actions">
                    <a class="btn primary large" href="/meetings">"Schedule a meeting"</a>
                </div>
            </section>

            <section class="join-strip">
                <h2>"Been sent a code?"</h2>
                <form class="join-form" on:submit=open_meeting>
                    <label class="visually-hidden" for="landing-code">"Meeting code"</label>
                    <input
                        id="landing-code"
                        type="text"
                        placeholder="Meeting code, like H7KD-PM2X-QR4T"
                        data-testid="landing-code-input"
                        prop:value=code
                        on:input=move |ev| code.set(event_target_value(&ev))
                    />
                    <button class="btn" type="submit" data-testid="landing-join-btn">
                        "Open the meeting"
                    </button>
                </form>
            </section>

            <footer class="site-foot">
                <p class="muted">
                    "A meeting link is the only thing a guest needs. Organizers \
                     sign in to schedule and cancel."
                </p>
            </footer>
        </main>
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
                Ok((200, text)) => {
                    // The "link is in the server log" hint only appears
                    // when the dev log mailer really delivered (issue
                    // #85); with SMTP/Resend configured the link goes to
                    // the mailbox and is never printed anywhere.
                    let message = if api::link_logged_in_server_log(&text) {
                        "Check your email — a sign-in link is on its way. \
                         (Running without SMTP? The link is printed in the server log.)"
                    } else {
                        "Check your email — a sign-in link is on its way."
                    };
                    status.set(Some(message.to_owned()))
                }
                Ok((_, text)) => {
                    // An OIDC-configured server refuses magic links with a
                    // 409 carrying the SSO entry point — follow it instead
                    // of showing the raw refusal body (issue #89).
                    if let Some(login) = api::oidc_login_redirect(&text)
                        && let Some(window) = web_sys::window()
                    {
                        let _ = window.location().assign(&login);
                        return;
                    }
                    status.set(Some(format!("Could not send the link: {text}")));
                }
                Err(e) => status.set(Some(format!("Could not send the link: {e}"))),
            }
        });
    };

    view! {
        <main class="page auth">
            <header class="site-nav">
                <a class="wordmark" href="/">
                    <span class="on-air-dot" aria-hidden="true"></span>
                    "huddle"
                </a>
            </header>
            <div class="card auth-card">
                <h1 class="page-title">"Sign in"</h1>
                <p class="muted">"We'll email you a one-time sign-in link. No passwords."</p>
                <form on:submit=submit>
                    <label class="field">
                        "Email"
                        <input
                            type="email"
                            placeholder="you@example.com"
                            data-testid="login-email"
                            prop:value=email
                            on:input=move |ev| email.set(event_target_value(&ev))
                        />
                    </label>
                    <button class="btn primary" type="submit" data-testid="login-submit">
                        "Email me a link"
                    </button>
                </form>
                {move || {
                    status
                        .get()
                        .map(|s| view! { <p data-testid="login-status" role="status">{s}</p> })
                }}
            </div>
        </main>
    }
}

/// The organizer dashboard: list, create, and manage the meetings you own.
#[cfg(target_arch = "wasm32")]
#[component]
fn Dashboard() -> impl IntoView {
    use huddle_protocol::{
        AuthorizationDto, CreateMeetingRequest, Joinability, MeetingResponse, RecurrenceRule,
        RepeatFrequency, UpdateMeetingRequest,
    };
    use time::format_description::well_known::Rfc3339;

    let organizer_email = RwSignal::<Option<String>>::new(None);
    let meetings = RwSignal::<Vec<MeetingResponse>>::new(Vec::new());
    let signed_out = RwSignal::new(false);
    let status = RwSignal::<Option<String>>::new(None);
    let title = RwSignal::new(String::new());
    let starts = RwSignal::new(String::new());
    let ends = RwSignal::new(String::new());
    let repeat = RwSignal::new("never".to_owned());
    let create_passcode = RwSignal::new(String::new());
    // The E2EE trust mode is chosen at creation (issue #27, ADR 010).
    let create_e2ee = RwSignal::new(false);
    // Agent authorizations (issue #9): the CLI/MCP refresh tokens this
    // organizer has approved through the OAuth device flow.
    let authorizations = RwSignal::<Vec<AuthorizationDto>>::new(Vec::new());

    // Refresh the owned-meetings list in the background. Encrypted
    // titles (issue #29) are decrypted per row: this is the
    // organizer's dashboard, and the title key is the organizer
    // identity's self-ECDH, so rows this browser created open; ones
    // from other devices keep the masked label.
    fn refresh(meetings: RwSignal<Vec<MeetingResponse>>) {
        spawn_local(async move {
            if let Ok((200, body)) = api::fetch_text("GET", "/api/meetings/mine", None).await
                && let Ok(mut list) = serde_json::from_str::<Vec<MeetingResponse>>(&body)
            {
                for row in &mut list {
                    if let Some(ciphertext) = row.meeting.title_ciphertext.clone()
                        && let Ok(Some(title)) =
                            crate::e2ee::titles::decrypt_title(&ciphertext).await
                    {
                        row.meeting.title = title;
                    }
                }
                meetings.set(list);
            }
        });
    }

    // Load the organizer's active agent authorizations. Session only on
    // the server (bearer tokens get 401 here), so a revoked CLI cannot
    // re-read this list.
    fn refresh_authorizations(authorizations: RwSignal<Vec<AuthorizationDto>>) {
        spawn_local(async move {
            if let Ok((200, body)) = api::fetch_text("GET", "/api/oauth/authorizations", None).await
                && let Ok(list) = serde_json::from_str::<Vec<AuthorizationDto>>(&body)
            {
                authorizations.set(list);
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
            refresh_authorizations(authorizations);
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
        let repeat_rule = match repeat.get_untracked().as_str() {
            "daily" => Some(RecurrenceRule {
                frequency: RepeatFrequency::Daily,
                until: None,
                count: None,
            }),
            "weekly" => Some(RecurrenceRule {
                frequency: RepeatFrequency::Weekly,
                until: None,
                count: None,
            }),
            "weekdays" => Some(RecurrenceRule {
                frequency: RepeatFrequency::Weekdays,
                until: None,
                count: None,
            }),
            "monthly" => Some(RecurrenceRule {
                frequency: RepeatFrequency::Monthly,
                until: None,
                count: None,
            }),
            _ => None,
        };
        let passcode = create_passcode.get_untracked().trim().to_owned();
        let mut request = CreateMeetingRequest {
            title: title.get_untracked().trim().to_owned(),
            starts_at,
            ends_at,
            repeat: repeat_rule,
            passcode: (!passcode.is_empty()).then_some(passcode),
            e2ee: create_e2ee.get_untracked(),
        };
        // Validate the *plaintext* shape before sealing it — the server
        // can no longer see the title to check (issue #29).
        if let Err(message) = request.validate() {
            status.set(Some(message.to_owned()));
            return;
        }
        status.set(Some("Creating…".to_owned()));
        spawn_local(async move {
            // E2EE meetings store the title as an enc1 blob sealed to
            // this browser's identity; plaintext meetings send it as-is.
            if request.e2ee {
                match crate::e2ee::titles::encrypt_title(&request.title).await {
                    Ok(blob) => request.title = blob,
                    Err(e) => {
                        status.set(Some(format!("Could not encrypt the title: {e}")));
                        return;
                    }
                }
            }
            let Ok(body) = serde_json::to_string(&request) else {
                status.set(Some("Could not encode the meeting.".to_owned()));
                return;
            };
            match api::fetch_text("POST", "/api/meetings", Some(body)).await {
                Ok((201, _)) => {
                    status.set(None);
                    title.set(String::new());
                    starts.set(String::new());
                    ends.set(String::new());
                    repeat.set("never".to_owned());
                    create_passcode.set(String::new());
                    create_e2ee.set(false);
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
    // Cancel a single date of a recurring series; the series stays live.
    let cancel_occurrence = move |code: String, starts_at: String| {
        spawn_local(async move {
            let path = format!("/api/meetings/{code}/occurrences/{starts_at}");
            match api::fetch_text("DELETE", &path, None).await {
                Ok((200, _)) => refresh(meetings),
                Ok((_, text)) => status.set(Some(format!("Could not cancel the date: {text}"))),
                Err(e) => status.set(Some(format!("Could not cancel the date: {e}"))),
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
    // Set (Some) or remove (None) a meeting's passcode. The tri-state
    // wire format does the heavy lifting: absent means unchanged, so a
    // single-field PATCH never touches the title or times.
    let set_passcode = move |code: String, passcode: Option<String>| {
        let message = if passcode.is_some() {
            "Passcode saved."
        } else {
            "Passcode removed."
        }
        .to_owned();
        let Ok(body) = serde_json::to_string(&UpdateMeetingRequest {
            passcode: passcode.map(Some),
            ..Default::default()
        }) else {
            status.set(Some("Could not encode the passcode.".to_owned()));
            return;
        };
        spawn_local(async move {
            match api::fetch_text("PATCH", &format!("/api/meetings/{code}"), Some(body)).await {
                Ok((200, _)) => {
                    status.set(Some(message));
                    refresh(meetings);
                }
                Ok((_, text)) => status.set(Some(format!("Could not save the passcode: {text}"))),
                Err(e) => status.set(Some(format!("Could not save the passcode: {e}"))),
            }
        });
    };
    // Revoke an agent authorization: the server deletes the refresh
    // token row by id, so the agent's next refresh fails immediately
    // (any access token it still holds lapses within minutes).
    let revoke_auth = move |id: Uuid| {
        spawn_local(async move {
            match api::fetch_text("DELETE", &format!("/api/oauth/authorizations/{id}"), None).await
            {
                Ok((200, _)) => {
                    status.set(Some("Authorization revoked.".to_owned()));
                    refresh_authorizations(authorizations);
                }
                Ok((_, text)) => status.set(Some(format!("Could not revoke: {text}"))),
                Err(e) => status.set(Some(format!("Could not revoke: {e}"))),
            }
        });
    };

    view! {
        <main class="page dashboard">
            <header class="site-nav">
                <a class="wordmark" href="/">
                    <span class="on-air-dot" aria-hidden="true"></span>
                    "huddle"
                </a>
                <nav class="site-links">
                    {move || {
                        organizer_email
                            .get()
                            .map(|email| view! { <span class="muted">"Signed in as "{email}</span> })
                    }}
                </nav>
            </header>

            <h1 class="page-title">"Your meetings"</h1>
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
                    <section class="panel">
                        <h2 class="section-title">"Schedule a meeting"</h2>
                        <form class="create-form" on:submit=create>
                            <label class="field">
                                "Title"
                                <input
                                    type="text"
                                    placeholder="e.g. Weekly sync"
                                    data-testid="create-title"
                                    prop:value=title
                                    on:input=move |ev| title.set(event_target_value(&ev))
                                />
                            </label>
                            <label class="field">
                                "Starts"
                                <input
                                    type="datetime-local"
                                    data-testid="create-starts"
                                    prop:value=starts
                                    on:input=move |ev| starts.set(event_target_value(&ev))
                                />
                            </label>
                            <label class="field">
                                "Ends"
                                <input
                                    type="datetime-local"
                                    data-testid="create-ends"
                                    prop:value=ends
                                    on:input=move |ev| ends.set(event_target_value(&ev))
                                />
                            </label>
                            <label class="field">
                                "Repeats"
                                <select
                                    data-testid="create-repeat"
                                    prop:value=repeat
                                    on:change=move |ev| repeat.set(event_target_value(&ev))
                                >
                                    <option value="never">"Never"</option>
                                    <option value="daily">"Daily"</option>
                                    <option value="weekly">"Weekly"</option>
                                    <option value="weekdays">"Weekdays (Mon–Fri)"</option>
                                    <option value="monthly">"Monthly"</option>
                                </select>
                                "Passcode (optional)"
                                <input
                                    type="password"
                                    placeholder="Leave empty for an open meeting"
                                    autocomplete="off"
                                    data-testid="create-passcode"
                                    prop:value=create_passcode
                                    on:input=move |ev| create_passcode.set(event_target_value(&ev))
                                />
                            </label>
                            <label class="field">
                                "End-to-end encryption"
                                <input
                                    type="checkbox"
                                    data-testid="create-e2ee"
                                    prop:checked=create_e2ee
                                    on:change=move |ev| create_e2ee.set(event_target_checked(&ev))
                                />
                                " Participants verify each other by a safety number; recording is disabled."
                            </label>
                            <button class="btn primary" type="submit" data-testid="create-btn">
                                "Create meeting"
                            </button>
                        </form>
                    </section>

                    <section class="panel">
                        <h2 class="section-title">"Upcoming and past"</h2>
                        <ul data-testid="mine-list" class="meeting-list">
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
                                    let starts = api::rfc3339_to_local(&starts);
                                    let ends = current
                                        .meeting
                                        .ends_at
                                        .format(&Rfc3339)
                                        .unwrap_or_default();
                                    let ends = api::rfc3339_to_local(&ends);
                                    let (state, state_class) = match &current.joinability {
                                        Joinability::Open => {
                                            ("open", "chip chip-open")
                                        }
                                        Joinability::NotYet { .. } => {
                                            ("not yet", "chip chip-not-yet")
                                        }
                                        Joinability::Ended => ("ended", "chip chip-ended"),
                                        Joinability::Cancelled => {
                                            ("cancelled", "chip chip-cancelled")
                                        }
                                    };
                                    // Recurring rows: a repeat label, the next
                                    // occurrence, and per-date cancel buttons.
                                    let repeat_label = current.meeting.repeat.as_ref().map(
                                        |rule| match rule.frequency {
                                            RepeatFrequency::Daily => "repeats daily",
                                            RepeatFrequency::Weekly => "repeats weekly",
                                            RepeatFrequency::Weekdays => {
                                                "repeats every weekday"
                                            }
                                            RepeatFrequency::Monthly => "repeats monthly",
                                        },
                                    );
                                    let next_occurrence = current
                                        .upcoming_occurrences
                                        .iter()
                                        .find(|o| !o.cancelled)
                                        .map(|o| {
                                            let raw = o
                                                .starts_at
                                                .format(&Rfc3339)
                                                .unwrap_or_default();
                                            api::rfc3339_to_local(&raw)
                                        });
                                    let occurrences_testid = format!("occurrences-{code}");
                                    // Each handler owns its own clones: the row
                                    // closure re-runs, so nothing may be captured
                                    // by borrow.
                                    let occurrence_items = current
                                        .upcoming_occurrences
                                        .iter()
                                        .enumerate()
                                        .map(|(index, o)| {
                                            let raw = o
                                                .starts_at
                                                .format(&Rfc3339)
                                                .unwrap_or_default();
                                            let local = api::rfc3339_to_local(&raw);
                                            let testid =
                                                format!("cancel-occurrence-{code}-{index}");
                                            if o.cancelled {
                                                view! {
                                                    <li class="occurrence-item muted">
                                                        {format!("{local} — cancelled")}
                                                    </li>
                                                }
                                                    .into_any()
                                            } else {
                                                let click = {
                                                    let code = code.clone();
                                                    let raw = raw.clone();
                                                    move |_| cancel_occurrence(code.clone(), raw.clone())
                                                };
                                                view! {
                                                    <li class="occurrence-item">
                                                        <span>{local}</span>
                                                        <button
                                                            class="btn danger small"
                                                            data-testid=testid
                                                            on:click=click
                                                        >
                                                            "Cancel this date"
                                                        </button>
                                                    </li>
                                                }
                                                    .into_any()
                                            }
                                        })
                                        .collect::<Vec<_>>();
                                    let show_occurrences = !current.meeting.cancelled
                                        && current.meeting.repeat.is_some();
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
                                    // Per-row passcode draft. The row closure
                                    // re-runs only when the list refreshes,
                                    // at which point the saved state is the
                                    // truth anyway.
                                    let draft = RwSignal::new(String::new());
                                    let passcode_input_testid = format!("passcode-input-{code}");
                                    let passcode_save_testid = format!("passcode-save-{code}");
                                    let passcode_remove_testid = format!("passcode-remove-{code}");
                                    let passcode_testid = format!("passcode-state-{code}");
                                    let save_passcode_click = {
                                        let code = code.clone();
                                        move |_| {
                                            let value = draft.get_untracked().trim().to_owned();
                                            if value.is_empty() {
                                                status.set(Some(
                                                    "Enter a passcode first.".to_owned(),
                                                ));
                                                return;
                                            }
                                            set_passcode(code.clone(), Some(value));
                                        }
                                    };
                                    let remove_passcode_click = {
                                        let code = code.clone();
                                        move |_| set_passcode(code.clone(), None)
                                    };
                                    let has_passcode = current.meeting.has_passcode;
                                    let is_e2ee = current.meeting.e2ee;
                                    let e2ee_testid = format!("e2ee-state-{code}");
                                    let row_testid = format!("meeting-row-{code}");
                                    let title_testid = format!("meeting-title-{code}");
                                    let join_href = format!("/m/{code}");
                                    let ics_href = format!("/api/meetings/{code}/ics");
                                    let ics_testid = format!("ics-{code}");
                                    let copy_testid = format!("copy-link-{code}");
                                    let cancelled = current.meeting.cancelled;
                                    view! {
                                        <li data-testid=row_testid class="meeting-row">
                                            <div class="meeting-row-main">
                                                <strong class="meeting-title" data-testid=title_testid>
                                                    {current.meeting.title.clone()}
                                                </strong>
                                                <p class="meeting-when">
                                                    {format!("{starts} to {ends}")}
                                                </p>
                                                <p class="meeting-code-line">
                                                    "Meeting code "
                                                    <code>{code}</code>
                                                </p>
                                                {repeat_label.map(|label| {
                                                    view! {
                                                        <p class="meeting-repeat muted">
                                                            {label}
                                                        </p>
                                                    }
                                                })}
                                                {show_occurrences.then(move || {
                                                    view! {
                                                        <div class="occurrence-list">
                                                            <p class="muted">
                                                                {match next_occurrence {
                                                                    Some(next) => {
                                                                        format!("Next: {next}")
                                                                    }
                                                                    None => {
                                                                        "No upcoming occurrences"
                                                                            .to_owned()
                                                                    }
                                                                }}
                                                            </p>
                                                            <ul data-testid=occurrences_testid>
                                                                {occurrence_items}
                                                            </ul>
                                                        </div>
                                                    }
                                                })}
                                            </div>
                                            <span class=state_class>{state}</span>
                                            <div class="meeting-row-passcode">
                                                <span
                                                    class="chip"
                                                    data-testid=passcode_testid
                                                >
                                                    {if has_passcode {
                                                        "Passcode on"
                                                    } else {
                                                        "Open"
                                                    }}
                                                </span>
                                                {is_e2ee.then(move || {
                                                    view! {
                                                        <span class="chip chip-e2ee" data-testid=e2ee_testid>
                                                            "E2EE"
                                                        </span>
                                                    }
                                                })}
                                                <input
                                                    type="password"
                                                    placeholder="New passcode"
                                                    autocomplete="off"
                                                    data-testid=passcode_input_testid
                                                    prop:value=draft
                                                    on:input=move |ev| {
                                                        draft.set(event_target_value(&ev))
                                                    }
                                                />
                                                <button
                                                    class="btn small"
                                                    data-testid=passcode_save_testid
                                                    on:click=save_passcode_click
                                                >
                                                    "Save"
                                                </button>
                                                {has_passcode.then(move || {
                                                    view! {
                                                        <button
                                                            class="btn small"
                                                            data-testid=passcode_remove_testid
                                                            on:click=remove_passcode_click
                                                        >
                                                            "Remove"
                                                        </button>
                                                    }
                                                })}
                                            </div>
                                            <div class="meeting-row-actions">
                                                <a class="btn primary small" href=join_href>
                                                    "Join"
                                                </a>
                                                <button
                                                    class="btn small"
                                                    data-testid=copy_testid
                                                    on:click=copy_click
                                                >
                                                    "Copy link"
                                                </button>
                                                <a
                                                    class="btn small"
                                                    href=ics_href
                                                    download
                                                    data-testid=ics_testid
                                                >
                                                    ".ics"
                                                </a>
                                                {(!cancelled).then(move || {
                                                    view! {
                                                        <button
                                                            class="btn danger small"
                                                            data-testid=cancel_testid
                                                            on:click=cancel_click
                                                        >
                                                            "Cancel"
                                                        </button>
                                                    }
                                                })}
                                            </div>
                                        </li>
                                    }
                                }}
                            </For>
                        </ul>
                    </section>

                    <section class="panel" data-testid="authorizations-panel">
                        <h2 class="section-title">"Agent authorizations"</h2>
                        <p class="muted">
                            "CLI and MCP tokens you approved through the device flow. Revoking one signs that agent out immediately."
                        </p>
                        {move || {
                            authorizations
                                .get()
                                .is_empty()
                                .then(|| {
                                    view! {
                                        <p class="muted" data-testid="authorizations-empty">
                                            "No agent authorizations yet."
                                        </p>
                                    }
                                })
                        }}
                        <ul data-testid="authorizations-list" class="meeting-list">
                            <For each={move || authorizations.get()} key=|a| a.id let:a>
                                {move || {
                                    // Keyed-`For` children are not re-rendered
                                    // when the key stays the same, so re-read
                                    // the signal and find the live item (same
                                    // pattern as the meeting rows above).
                                    let current = authorizations
                                        .get()
                                        .into_iter()
                                        .find(|q| q.id == a.id)
                                        .unwrap_or_else(|| a.clone());
                                    let id = current.id;
                                    let scopes = current
                                        .scopes
                                        .iter()
                                        .map(|s| s.to_string())
                                        .collect::<Vec<_>>()
                                        .join(" ");
                                    let created_raw = current
                                        .created_at
                                        .format(&Rfc3339)
                                        .unwrap_or_default();
                                    let created = api::rfc3339_to_local(&created_raw);
                                    let last_used = current.last_used_at.map(|at| {
                                        let raw = at.format(&Rfc3339).unwrap_or_default();
                                        api::rfc3339_to_local(&raw)
                                    });
                                    let row_testid = format!("authorization-row-{id}");
                                    let revoke_testid = format!("revoke-auth-{id}");
                                    let revoke_click = move |_| revoke_auth(id);
                                    view! {
                                        <li data-testid=row_testid class="meeting-row">
                                            <div class="meeting-row-main">
                                                <strong class="meeting-title">
                                                    {current.client_name.clone()}
                                                </strong>
                                                <p class="meeting-code-line">
                                                    "Scopes "
                                                    <code>{scopes}</code>
                                                </p>
                                                <p class="meeting-repeat muted">
                                                    "Approved "{created}
                                                    {last_used
                                                        .map(|at| format!(" · last used {at}"))}
                                                </p>
                                            </div>
                                            <div class="meeting-row-actions">
                                                <button
                                                    class="btn danger small"
                                                    data-testid=revoke_testid
                                                    on:click=revoke_click
                                                >
                                                    "Revoke"
                                                </button>
                                            </div>
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
