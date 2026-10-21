//! Main-thread glue for the E2EE media transform worker (issue #28,
//! ADR 011, D1). The web-sys bindings for `RTCRtpScriptTransform` are
//! gated behind an unstable-cfg the wasm gate cannot enable, so every
//! interaction with the API is raw JS interop here: feature detection,
//! constructing one handle per sender/receiver, and posting derived
//! SFrame key material to the worker. The worker asset does all
//! per-frame work.

use crate::e2ee::sframe;
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{MessageEvent, RtcRtpReceiver, RtcRtpSender, Worker};

/// Same-origin URL of the vendored worker (never a Blob: the CSP's
/// worker-src falls back to script-src 'self', which admits URLs only
/// — ADR 011, D1).
pub const WORKER_URL: &str = "/assets/e2ee/e2ee-worker.js";

/// The latest transform stats snapshot posted by the worker.
#[derive(Clone, Copy, Default, Debug)]
pub struct TransformStats {
    pub attached: f64,
    pub encrypted_frames: f64,
    pub decrypted_frames: f64,
    pub decrypt_fail: f64,
    pub ema_ms: f64,
    pub degraded: bool,
}

impl TransformStats {
    fn update_from(&mut self, data: &js_sys::Object) {
        let get = |key: &str| {
            js_sys::Reflect::get(data, &JsValue::from_str(key))
                .ok()
                .and_then(|value| value.as_f64())
                .unwrap_or_default()
        };
        self.attached = get("attached");
        self.encrypted_frames = get("encryptedFrames");
        self.decrypted_frames = get("decryptedFrames");
        self.decrypt_fail = get("decryptFail");
        self.ema_ms = get("emaMs");
        self.degraded = js_sys::Reflect::get(data, &JsValue::from_str("degraded"))
            .ok()
            .is_some_and(|value| value.as_bool().unwrap_or(false));
    }
}

/// Feature detection (ADR 011, D6): an incapable browser is refused an
/// E2EE meeting at join rather than sending media nobody could decrypt.
pub fn supports_e2ee() -> bool {
    let Some(window) = web_sys::window() else {
        return false;
    };
    let win: JsValue = window.into();
    let has = |name: &str| {
        js_sys::Reflect::get(&win, &JsValue::from_str(name))
            .map(|value| value.is_function())
            .unwrap_or(false)
    };
    if !has("RTCRtpScriptTransform") || !has("Worker") {
        return false;
    }
    // The `transform` attribute must exist on both sender and receiver.
    for iface in ["RTCRtpSender", "RTCRtpReceiver"] {
        let class = js_sys::Reflect::get(&win, &JsValue::from_str(iface));
        let Ok(class) = class else { return false };
        let proto = js_sys::Reflect::get(&class, &JsValue::from_str("prototype"));
        let Ok(proto) = proto else { return false };
        if !js_sys::Reflect::has(&proto, &JsValue::from_str("transform")).unwrap_or(false) {
            return false;
        }
    }
    true
}

fn set(obj: &js_sys::Object, key: &str, value: &JsValue) {
    let _ = js_sys::Reflect::set(obj, &JsValue::from_str(key), value);
}

type DegradeCallback = Rc<RefCell<Option<Box<dyn FnMut()>>>>;

/// The worker handle: cheaply cloneable (the stats and the message
/// handler are shared behind `Rc`).
#[derive(Clone)]
pub struct TransformWorker {
    worker: Worker,
    _on_message: Rc<Closure<dyn FnMut(MessageEvent)>>,
    stats: Rc<RefCell<TransformStats>>,
    degrade: DegradeCallback,
}

impl TransformWorker {
    /// Start the worker and arm the test knobs (ADR 011, D10).
    pub fn new(tag_only: bool, force_degrade: bool) -> Result<Self, String> {
        let worker = Worker::new(WORKER_URL).map_err(|e| format!("cannot start worker: {e:?}"))?;
        let stats = Rc::new(RefCell::new(TransformStats::default()));
        let degrade: DegradeCallback = Rc::new(RefCell::new(None));
        let stats_for_handler = Rc::clone(&stats);
        let degrade_for_handler = Rc::clone(&degrade);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let Ok(data) = event.data().dyn_into::<js_sys::Object>() else {
                return;
            };
            let kind = js_sys::Reflect::get(data.as_ref(), &JsValue::from_str("t"));
            let is_stats = kind
                .as_ref()
                .is_ok_and(|kind| kind == &JsValue::from_str("stats"));
            let is_degrade = kind
                .as_ref()
                .is_ok_and(|kind| kind == &JsValue::from_str("degrade"));
            if is_stats {
                stats_for_handler.borrow_mut().update_from(&data);
            } else if is_degrade {
                stats_for_handler.borrow_mut().degraded = true;
                let mut callback = degrade_for_handler.borrow_mut();
                if let Some(notify) = callback.as_mut() {
                    notify();
                }
            }
        });
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        let me = Self {
            worker,
            _on_message: Rc::new(on_message),
            stats,
            degrade,
        };
        let config = js_sys::Object::new();
        set(&config, "t", &JsValue::from_str("config"));
        set(&config, "tagOnly", &JsValue::from_bool(tag_only));
        set(&config, "forceDegrade", &JsValue::from_bool(force_degrade));
        me.post(&config);
        Ok(me)
    }

    fn post(&self, message: &js_sys::Object) {
        let _ = self.worker.post_message(message.as_ref());
    }

    /// Push the full key table to the worker (a versioned whole-table
    /// replace — ADR 011, D7 sync). The SFrame key/salt derivation runs
    /// here in Rust, pinned against the RFC vectors; the worker only
    /// imports and uses the bytes.
    pub fn post_keys(
        &self,
        version: u64,
        send: Option<(u32, [u8; sframe::KEY_LEN])>,
        recv: &[(u32, [u8; sframe::KEY_LEN])],
    ) {
        let msg = js_sys::Object::new();
        set(&msg, "t", &JsValue::from_str("keys"));
        set(&msg, "version", &JsValue::from_f64(version as f64));
        let entry = |kid: u32, base: &[u8; sframe::KEY_LEN]| {
            let (key, salt) = sframe::key_salt(base, kid);
            let entry = js_sys::Object::new();
            set(&entry, "kid", &JsValue::from_f64(kid as f64));
            set(&entry, "key", js_sys::Uint8Array::from(&key[..]).as_ref());
            set(&entry, "salt", js_sys::Uint8Array::from(&salt[..]).as_ref());
            entry
        };
        if let Some((kid, base)) = send {
            set(&msg, "send", entry(kid, &base).as_ref());
        }
        let list = js_sys::Array::new();
        for (kid, base) in recv {
            list.push(&entry(*kid, base));
        }
        set(&msg, "recv", list.as_ref());
        self.post(&msg);
    }

    fn construct(&self, op: &str, kind: &str) -> Result<JsValue, String> {
        let Some(window) = web_sys::window() else {
            return Err("no window".to_owned());
        };
        let ctor =
            js_sys::Reflect::get(&window.into(), &JsValue::from_str("RTCRtpScriptTransform"))
                .map_err(|_| "RTCRtpScriptTransform unavailable".to_owned())?;
        let options = js_sys::Object::new();
        set(&options, "op", &JsValue::from_str(op));
        set(&options, "kind", &JsValue::from_str(kind));
        let args = js_sys::Array::new();
        let worker_ref: &JsValue = self.worker.as_ref();
        args.push(worker_ref);
        args.push(&options);
        js_sys::Reflect::construct(ctor.unchecked_ref::<js_sys::Function>(), &args)
            .map_err(|e| format!("cannot construct the transform: {e:?}"))
    }

    /// Bind an encrypting transform to one sender.
    pub fn attach_to_sender(&self, sender: &RtcRtpSender, kind: &str) -> Result<(), String> {
        let transform = self.construct("encrypt", kind)?;
        js_sys::Reflect::set(sender.as_ref(), &JsValue::from_str("transform"), &transform)
            .map(|_| ())
            .map_err(|e| format!("cannot attach to the sender: {e:?}"))
    }

    /// Bind a decrypting transform to one receiver.
    pub fn attach_to_receiver(&self, receiver: &RtcRtpReceiver, kind: &str) -> Result<(), String> {
        let transform = self.construct("decrypt", kind)?;
        js_sys::Reflect::set(
            receiver.as_ref(),
            &JsValue::from_str("transform"),
            &transform,
        )
        .map(|_| ())
        .map_err(|e| format!("cannot attach to the receiver: {e:?}"))
    }

    /// The callback fired (once) when the worker's EMA degrade detector
    /// trips (ADR 011, D5).
    pub fn set_on_degrade(&self, notify: impl FnMut() + 'static) {
        *self.degrade.borrow_mut() = Some(Box::new(notify));
    }

    /// The latest worker stats.
    pub fn stats(&self) -> TransformStats {
        *self.stats.borrow()
    }
}
