//! Explicit echo-cancellation / noise-suppression constraints for mic
//! acquisition, and the user controls that drive them (issue #34).
//!
//! Echo suppression is strictly client-side: the SFU forwards encrypted
//! media and never decodes audio, so the browser's built-in AEC is the
//! only line of defense. This module makes that defense explicit —
//! every getUserMedia applies `echoCancellation` / `noiseSuppression` /
//! `autoGainControl` from an [`AudioSettings`] value — and persists the
//! user's choices per browser via localStorage.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;

/// The audio processing constraints applied to every mic acquisition.
/// Constraints are acquisition-time: a change takes effect when the
/// mic is (re-)acquired, so both the prejoin and the in-call toggles
/// re-acquire the mic with the fresh value right away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSettings {
    /// The browser's built-in acoustic echo canceller. The one that
    /// keeps speakers from feeding back into the mic.
    pub echo_cancellation: bool,
    /// The browser's built-in noise suppressor (hum, keyboard, fan).
    pub noise_suppression: bool,
    // `autoGainControl` is always applied (see `mic_constraint`) but
    // has no toggle yet; the field set stays open for one.
}

impl Default for AudioSettings {
    /// Both processors on: the right default for laptop speakers and
    /// the least surprise for a fresh browser.
    fn default() -> Self {
        Self {
            echo_cancellation: true,
            noise_suppression: true,
        }
    }
}

/// The localStorage key the settings persist under.
const STORAGE_KEY: &str = "huddle.audio-settings";

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

/// Load the persisted settings, falling back to the defaults for a
/// fresh browser (or a storage area that cannot be read, e.g. blocked
/// third-party storage — never a join blocker).
pub fn load() -> AudioSettings {
    local_storage()
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok().flatten())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist `settings` per browser. Best-effort: failing to write is
/// not a reason to interrupt a call.
pub fn save(settings: &AudioSettings) {
    if let Some(storage) = local_storage()
        && let Ok(raw) = serde_json::to_string(settings)
    {
        let _ = storage.set_item(STORAGE_KEY, &raw);
    }
}

/// The getUserMedia audio constraint: the picked device (if any) plus
/// the explicit processing constraints. Mirrors the `{ deviceId: { exact:
/// id } }` shape `media` uses for the camera, so device selection and
/// processing live in one constraint dictionary on one acquisition.
pub fn mic_constraint(mic_id: Option<&str>, settings: AudioSettings) -> JsValue {
    let constraint = js_sys::Object::new();
    if let Some(id) = mic_id {
        let exact = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&exact, &JsValue::from_str("exact"), &JsValue::from_str(id));
        let _ = js_sys::Reflect::set(
            &constraint,
            &JsValue::from_str("deviceId"),
            &JsValue::from(exact),
        );
    }
    // AGC is always explicit (on): without a toggle the browser's
    // default-of-the-day would silently decide, which is exactly what
    // issue #34 is about.
    for (name, on) in [
        ("echoCancellation", settings.echo_cancellation),
        ("noiseSuppression", settings.noise_suppression),
        ("autoGainControl", true),
    ] {
        let _ = js_sys::Reflect::set(&constraint, &JsValue::from_str(name), &JsValue::from(on));
    }
    constraint.into()
}

/// The AEC / noise-suppression toggles, shared by the prejoin panel
/// and the in-call controls. A flip updates the settings signal,
/// persists the choice per browser, and hands the fresh value to
/// `on_change` — the page decides what re-acquisition means there
/// (prejoin refreshes the preview; in-call swaps the live track).
#[component]
pub fn AudioSettingsControls(
    settings: RwSignal<AudioSettings>,
    on_change: Callback<AudioSettings>,
) -> impl IntoView {
    let flip = move |field: fn(&mut AudioSettings, bool), on_change: &Callback<AudioSettings>| {
        let on_change = *on_change;
        move |ev| {
            let checked = event_target_checked(&ev);
            settings.update(|s| field(s, checked));
            let fresh = settings.get_untracked();
            save(&fresh);
            on_change.run(fresh);
        }
    };
    let on_echo = flip(|s, on| s.echo_cancellation = on, &on_change);
    let on_noise = flip(|s, on| s.noise_suppression = on, &on_change);

    view! {
        <div class="audio-settings" data-testid="audio-settings">
            <label class="audio-setting">
                <input
                    type="checkbox"
                    data-testid="toggle-echo-cancellation"
                    prop:checked=move || settings.get().echo_cancellation
                    on:change=on_echo
                />
                "Echo cancellation"
            </label>
            <label class="audio-setting">
                <input
                    type="checkbox"
                    data-testid="toggle-noise-suppression"
                    prop:checked=move || settings.get().noise_suppression
                    on:change=on_noise
                />
                "Noise suppression"
            </label>
        </div>
    }
}
