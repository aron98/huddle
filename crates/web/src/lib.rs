#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! The huddle web client.
//!
//! Leptos CSR app: the prejoin page and (later) the in-call UI.

use leptos::prelude::*;

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

/// Placeholder prejoin page; gains camera preview and device pickers in the
/// next commits.
#[component]
fn Prejoin(code: String) -> impl IntoView {
    view! {
        <main>
            <h1>"Join meeting"</h1>
            <p>{format!("Meeting code: {code}")}</p>
        </main>
    }
}
