//! Background blur for the local camera feed.
//!
//! The effects module owns a persistent "outgoing" stream: the raw audio
//! track plus whichever video track is current — the blurred canvas
//! track while blur is on, the raw camera track otherwise. Video track
//! swaps happen in place on the same `MediaStream`, so preview bindings
//! and the published sender survive toggles.
//!
//! The heavy lifting (MediaPipe selfie segmentation, per-frame
//! compositing, `canvas.captureStream`) lives in the vendored
//! `/assets/effects/blur.js` shim; this module orchestrates it, keeps
//! the user's choice in `localStorage`, and degrades to raw video when
//! the model fails to load or frames become too slow.

use std::cell::RefCell;
use std::rc::Rc;

use futures::channel::oneshot;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{HtmlVideoElement, MediaStream, MediaStreamTrack};

const SHIM_URL: &str = "/assets/effects/blur.js";
const BLUR_PREF_KEY: &str = "huddle-blur";

thread_local! {
    /// `window.__huddleBlur` once the shim script has loaded.
    static SHIM: RefCell<Option<JsValue>> = const { RefCell::new(None) };
    /// Callback fired whenever the outgoing video track changes (used
    /// by the meeting session to `replaceTrack` the new one in place).
    static ON_CHANGE: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    /// The auto-degrade callback handed to the shim. One slot per page:
    /// the meeting flow is a single route.
    static DEGRADE: RefCell<Option<Closure<dyn FnMut()>>> = const { RefCell::new(None) };
}

/// A live blur pipeline: the shim's stop function and the hidden video
/// element feeding the segmenter (both needed for teardown).
#[derive(Clone)]
struct Pipeline {
    stop: js_sys::Function,
    driver: HtmlVideoElement,
}

/// Copyable handle to the background-blur state for one page.
///
/// The raw camera stream is the "source"; the "outgoing" stream is what
/// prejoin previews and the call publishes. Both `PrejoinState` and the
/// meeting signals pass this handle around by value.
#[derive(Clone, Copy)]
pub struct Effects {
    /// Whether blur is currently applied to the outgoing video.
    pub blur_on: RwSignal<bool>,
    /// Last pipeline failure / auto-disable reason, if any.
    pub notice: RwSignal<Option<String>>,
    source: RwSignal<Option<MediaStream>>,
    outgoing: RwSignal<Option<MediaStream>>,
    pipeline: RwSignal<Option<Pipeline>>,
    busy: RwSignal<bool>,
    test: bool,
}

impl Default for Effects {
    fn default() -> Self {
        Self::new()
    }
}

impl Effects {
    /// New effects state. The blur preference is seeded from
    /// `localStorage` so a prejoin reload keeps the user's choice. Under
    /// `?test=1` a `window.__huddleEffectsStats` object is created for
    /// e2e assertions (Rust owns `active`/`notice`/`error`; the shim
    /// owns `framesIn`/`framesOut`/`avgMs`/`degraded`).
    pub fn new() -> Self {
        let window = web_sys::window();
        let test = window
            .as_ref()
            .and_then(|w| w.location().search().ok())
            .is_some_and(|s| s.contains("test=1"));
        let effects = Self {
            blur_on: RwSignal::new(persisted_blur()),
            notice: RwSignal::new(None),
            source: RwSignal::new(None),
            outgoing: RwSignal::new(None),
            pipeline: RwSignal::new(None),
            busy: RwSignal::new(false),
            test,
        };
        if test && let Some(window) = window {
            let stats = js_sys::Object::new();
            let fields: [(&str, JsValue); 7] = [
                ("active", JsValue::from_bool(false)),
                ("notice", JsValue::NULL),
                ("error", JsValue::NULL),
                ("framesIn", JsValue::from_f64(0.0)),
                ("framesOut", JsValue::from_f64(0.0)),
                ("avgMs", JsValue::from_f64(0.0)),
                ("degraded", JsValue::from_bool(false)),
            ];
            for (name, value) in fields {
                let _ = js_sys::Reflect::set(&stats, &JsValue::from_str(name), &value);
            }
            let _ = js_sys::Reflect::set(
                window.as_ref(),
                &JsValue::from_str("__huddleEffectsStats"),
                &stats,
            );
        }
        effects
    }

    /// Point the pipeline at a (possibly new) camera stream. No-op when
    /// it is the same stream object: prejoin re-acquires on every device
    /// change, while the meeting flow just attaches the prejoin stream.
    pub fn attach_source(&self, stream: &MediaStream) {
        let same = self
            .source
            .get_untracked()
            .is_some_and(|old| old == *stream);
        if same {
            return;
        }
        let had_pipeline = self.pipeline.get_untracked().is_some();
        // NB: web-sys `Clone` for `MediaStream` lowers to the JS
        // `MediaStream.clone()` method — a TRACK SNAPSHOT: the cloned
        // stream carries NEW, LIVE track objects. A cloned mic track
        // keeps the device capture open under the previous
        // audio-processing config, which pins Chrome's per-device
        // processing state: every later getUserMedia on that device
        // silently inherits the old config instead of the requested
        // one (observed as the in-call AEC toggle failing until the
        // capture actually ends). Publish the SAME stream object by
        // cloning the JsValue handle (a refcount bump), not the
        // stream itself.
        let raw: JsValue = <MediaStream as AsRef<JsValue>>::as_ref(stream).clone();
        let handle = MediaStream::from(raw);
        self.source.set(Some(handle));
        self.rebuild_outgoing();
        if had_pipeline {
            // Camera changed mid-call: drop the pipeline (it reads the
            // stale stream) and bring blur back on the new source.
            let this = *self;
            spawn_local(async move {
                this.teardown();
                if this.blur_on.get_untracked() {
                    let _ = this.set_blur(true).await;
                }
            });
        }
    }

    /// Recreate the outgoing stream contents from the raw source. The
    /// stream object itself stays stable so bindings keep working.
    fn rebuild_outgoing(&self) {
        let Some(source) = self.source.get_untracked() else {
            return;
        };
        let outgoing = match self.outgoing.get_untracked() {
            Some(existing) => existing,
            None => match MediaStream::new() {
                Ok(fresh) => {
                    // NB: web-sys `Clone` for `MediaStream` lowers to the
                    // JS `MediaStream.clone()` method — a TRACK SNAPSHOT,
                    // not a reference clone. Publishing `fresh.clone()`
                    // would put a permanently-empty snapshot in the signal
                    // while the tracks land on a dropped object. Publish
                    // `fresh` itself and read the handle back so the
                    // signal and the stream we mutate below are the same
                    // object.
                    self.outgoing.set(Some(fresh));
                    match self.outgoing.get_untracked() {
                        Some(handle) => handle,
                        None => return,
                    }
                }
                Err(_) => return, // no MediaStream construction in practice
            },
        };
        for track in crate::media::tracks_of(&outgoing) {
            outgoing.remove_track(&track);
        }
        for track in crate::media::tracks_of(&source) {
            outgoing.add_track(&track);
        }
    }

    /// The outgoing stream (raw audio + current video track). `None`
    /// until a camera stream has been attached.
    pub fn outgoing(&self) -> Option<MediaStream> {
        self.outgoing.get_untracked()
    }

    /// Like [`Effects::outgoing`], but reactive: for preview effects
    /// that must re-run when the outgoing stream first appears.
    pub fn outgoing_tracked(&self) -> Option<MediaStream> {
        self.outgoing.get()
    }

    /// The video track the sender should currently carry — what
    /// `replaceTrack` gets on every blur transition.
    pub fn video_out(&self) -> Option<MediaStreamTrack> {
        self.outgoing
            .get_untracked()
            .and_then(|stream| video_track_of(&stream))
    }

    /// Enable/disable the RAW camera track. Mute toggles act on the
    /// source (not the processed track) so join-time mute seeding and
    /// the shim's black-fill behaviour stay consistent.
    pub fn set_camera_enabled(&self, enabled: bool) {
        if let Some(source) = self.source.get_untracked() {
            for track in crate::media::tracks_of(&source) {
                if track.kind() == "video" {
                    track.set_enabled(enabled);
                }
            }
        }
    }

    /// Register the callback fired whenever the outgoing video track
    /// changes (blur on/off/auto-disable).
    pub fn set_on_change(&self, callback: impl FnMut() + 'static) {
        ON_CHANGE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(callback));
        });
    }

    fn fire_on_change(&self) {
        ON_CHANGE.with(|slot| {
            let mut guard = slot.borrow_mut();
            if let Some(callback) = guard.as_mut() {
                callback();
            }
        });
    }

    /// Toggle blur. `Ok` on success; `Err` carries the user-facing
    /// reason (also set into `notice`). A failed toggle always leaves
    /// raw video flowing — it never unwinds the call.
    pub async fn set_blur(self, on: bool) -> Result<(), String> {
        if self.busy.get_untracked() {
            return Ok(()); // a toggle is already in flight
        }
        if self.pipeline.get_untracked().is_some() == on {
            return Ok(()); // already there
        }
        self.busy.set(true);
        let result = if on {
            self.activate().await
        } else {
            self.deactivate(None);
            Ok(())
        };
        self.busy.set(false);
        match result {
            Ok(()) => {
                self.set_notice(None);
                self.stats_set("error", &JsValue::NULL);
                Ok(())
            }
            Err(reason) => {
                self.set_notice(Some(reason.clone()));
                self.stats_set("error", &JsValue::from_str(&reason));
                Err(reason)
            }
        }
    }

    /// Build the pipeline: load the shim, initialize the model, start
    /// the per-frame loop, and swap the processed track into outgoing.
    async fn activate(self) -> Result<(), String> {
        let Some(source) = self.source.get_untracked() else {
            return Err("Turn on your camera to blur your background.".to_owned());
        };
        let Some(raw_video) = video_track_of(&source) else {
            return Err("Turn on your camera to blur your background.".to_owned());
        };

        let shim = self.ensure_shim().await?;
        let init = js_sys::Reflect::get(&shim, &JsValue::from_str("init"))
            .ok()
            .and_then(|value| value.dyn_into::<js_sys::Function>().ok())
            .ok_or_else(|| "the blur runtime is unavailable".to_owned())?;
        let init = init
            .call0(&shim)
            .map_err(|_| "the blur model could not be loaded".to_owned())?;
        let init = init
            .dyn_into::<js_sys::Promise>()
            .map_err(|_| "the blur model could not be loaded".to_owned())?;
        JsFuture::from(init)
            .await
            .map_err(|_| "the blur model could not be loaded".to_owned())?;

        // A hidden video feeds the segmenter; the preview/published
        // stream keeps the raw audio + processed video.
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return Err("the camera preview could not start".to_owned());
        };
        let driver = document
            .create_element("video")
            .ok()
            .and_then(|element| element.dyn_into::<HtmlVideoElement>().ok())
            .ok_or_else(|| "the camera preview could not start".to_owned())?;
        driver.set_muted(true);
        let _ = driver.set_attribute("playsinline", "");
        let _ = driver.set_attribute("style", "display:none");
        driver.set_src_object(Some(&source));
        if let Some(body) = document.body() {
            let _ = body.append_child(&driver);
        }
        let play = driver
            .play()
            .map_err(|_| "the camera preview could not start".to_owned())?;
        JsFuture::from(play)
            .await
            .map_err(|_| "the camera preview could not start".to_owned())?;

        let started = DEGRADE.with(|slot| {
            let mut guard = slot.borrow_mut();
            let degrade: &Closure<dyn FnMut()> = guard.get_or_insert_with(|| {
                let this = self;
                Closure::<dyn FnMut()>::new(move || this.handle_degrade())
            });
            let start = js_sys::Reflect::get(&shim, &JsValue::from_str("start"))
                .ok()
                .and_then(|value| value.dyn_into::<js_sys::Function>().ok());
            let Some(start) = start else {
                return Err("the blur runtime is unavailable".to_owned());
            };
            start
                .call3(&shim, &driver, &raw_video, degrade.as_ref())
                .map_err(|e| format!("background blur could not start: {e:?}"))
        });
        let handle = started?
            .dyn_into::<js_sys::Promise>()
            .map_err(|_| "background blur could not start: promise expected".to_owned())?;
        let handle = JsFuture::from(handle)
            .await
            .map_err(|e| format!("background blur could not start: {e:?}"))?;

        // From here the JS pipeline is running: any failure below must
        // tear it back down before returning.
        let track = js_sys::Reflect::get(&handle, &JsValue::from_str("track"))
            .ok()
            .and_then(|value| value.dyn_into::<MediaStreamTrack>().ok());
        let stop = js_sys::Reflect::get(&handle, &JsValue::from_str("stop"))
            .ok()
            .and_then(|value| value.dyn_into::<js_sys::Function>().ok());
        let (Some(track), Some(stop)) = (track, stop) else {
            if let Ok(stop) = js_sys::Reflect::get(&handle, &JsValue::from_str("stop"))
                .and_then(|value| value.dyn_into::<js_sys::Function>())
            {
                let _ = stop.call0(&JsValue::NULL);
            }
            if let Some(parent) = driver.parent_node() {
                let _ = parent.remove_child(&driver);
            }
            return Err("the blur runtime is unavailable".to_owned());
        };

        self.pipeline.set(Some(Pipeline { stop, driver }));
        self.set_outgoing_video(&track);
        self.blur_on.set(true);
        persist(true);
        self.stats_set("active", &JsValue::from_bool(true));
        self.fire_on_change();
        Ok(())
    }

    /// Stop the pipeline (if any) and restore raw video. `notice` is
    /// the user-facing reason for the deactivation (auto-disable), or
    /// `None` for a deliberate toggle off.
    fn deactivate(self, notice: Option<String>) {
        self.teardown();
        self.restore_raw_video();
        self.blur_on.set(false);
        persist(false);
        self.stats_set("active", &JsValue::from_bool(false));
        self.set_notice(notice);
        self.fire_on_change();
    }

    /// Stop the JS pipeline and drop the hidden driver video. Does not
    /// touch any signals — used by `deactivate` and source swaps.
    fn teardown(&self) {
        let current = self.pipeline.get_untracked();
        self.pipeline.set(None);
        if let Some(pipeline) = current {
            let _ = pipeline.stop.call0(&JsValue::NULL);
            if let Some(parent) = pipeline.driver.parent_node() {
                let _ = parent.remove_child(&pipeline.driver);
            }
            pipeline.driver.set_src_object(None);
        }
    }

    /// Called synchronously from JS when the shim decides frames are
    /// too slow: turn blur off and explain why.
    fn handle_degrade(self) {
        if self.busy.get_untracked() || self.pipeline.get_untracked().is_none() {
            return;
        }
        self.deactivate(Some(
            "Background blur was turned off to keep video smooth.".to_owned(),
        ));
    }

    /// Swap the outgoing video track in place (processed in, raw out),
    /// keeping the audio track and the stream object untouched.
    fn set_outgoing_video(&self, track: &MediaStreamTrack) {
        let Some(outgoing) = self.outgoing.get_untracked() else {
            return;
        };
        for old in crate::media::tracks_of(&outgoing) {
            if old.kind() == "video" {
                outgoing.remove_track(&old);
            }
        }
        outgoing.add_track(track);
    }

    /// Put the raw camera track back into outgoing (after teardown).
    fn restore_raw_video(&self) {
        if let Some(source) = self.source.get_untracked()
            && let Some(raw) = video_track_of(&source)
        {
            self.set_outgoing_video(&raw);
        }
    }

    /// Load `/assets/effects/blur.js` once and return its
    /// `window.__huddleBlur` handle.
    async fn ensure_shim(self) -> Result<JsValue, String> {
        if let Some(shim) = SHIM.with(|slot| slot.borrow().clone()) {
            return Ok(shim);
        }
        let Some(window) = web_sys::window() else {
            return Err("the blur effect could not be loaded".to_owned());
        };
        let Some(document) = window.document() else {
            return Err("the blur effect could not be loaded".to_owned());
        };
        let Some(head) = document.head() else {
            return Err("the document has no head".to_owned());
        };

        let script = document
            .create_element("script")
            .map_err(|_| "the blur effect could not be loaded".to_owned())?;
        let _ = script.set_attribute("src", SHIM_URL);

        let (tx, rx) = oneshot::channel::<bool>();
        let tx = Rc::new(RefCell::new(Some(tx)));
        let on_load = {
            let tx = Rc::clone(&tx);
            Closure::<dyn FnMut()>::new(move || {
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(true);
                }
            })
        };
        let on_error = {
            let tx = Rc::clone(&tx);
            Closure::<dyn FnMut()>::new(move || {
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(false);
                }
            })
        };
        let _ = script.add_event_listener_with_callback(
            "load",
            on_load.as_ref().unchecked_ref::<js_sys::Function>(),
        );
        let _ = script.add_event_listener_with_callback(
            "error",
            on_error.as_ref().unchecked_ref::<js_sys::Function>(),
        );
        // Same-origin <script> load is one-shot per page; the closures
        // stay alive for the page lifetime (bounded leak, same
        // precedent as prejoin's poll_until_open).
        std::mem::forget((on_load, on_error));
        let _ = head.append_child(&script);

        let loaded = rx
            .await
            .map_err(|_| "the blur effect could not be loaded".to_owned())?;
        if !loaded {
            return Err("the blur effect could not be loaded".to_owned());
        }
        let shim = js_sys::Reflect::get(window.as_ref(), &JsValue::from_str("__huddleBlur"))
            .ok()
            .ok_or_else(|| "the blur runtime did not initialize".to_owned())?;
        if shim.is_null() || shim.is_undefined() {
            return Err("the blur runtime did not initialize".to_owned());
        }
        SHIM.with(|slot| *slot.borrow_mut() = Some(shim.clone()));
        Ok(shim)
    }

    /// Set `notice` and mirror it into the e2e stats object.
    fn set_notice(&self, notice: Option<String>) {
        self.notice.set(notice.clone());
        let mirrored = match notice {
            Some(text) => JsValue::from_str(&text),
            None => JsValue::NULL,
        };
        self.stats_set("notice", &mirrored);
    }

    /// Write one e2e stats field (test mode only).
    fn stats_set(&self, name: &str, value: &JsValue) {
        if !self.test {
            return;
        }
        if let Some(window) = web_sys::window()
            && let Ok(stats) =
                js_sys::Reflect::get(window.as_ref(), &JsValue::from_str("__huddleEffectsStats"))
            && !stats.is_null()
            && !stats.is_undefined()
        {
            let _ = js_sys::Reflect::set(&stats, &JsValue::from_str(name), value);
        }
    }
}

/// The video track of `stream`, if any.
fn video_track_of(stream: &MediaStream) -> Option<MediaStreamTrack> {
    crate::media::tracks_of(stream)
        .into_iter()
        .find(|track| track.kind() == "video")
}

/// Persist the blur choice across prejoin reloads.
fn persist(on: bool) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(BLUR_PREF_KEY, if on { "on" } else { "off" });
    }
}

fn persisted_blur() -> bool {
    web_sys::window()
        .and_then(|w| w.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(BLUR_PREF_KEY).ok().flatten())
        .is_some_and(|value| value == "on")
}
