//! The long-term browser identity (ADR 010, D1).
//!
//! One ECDH P-256 keypair per browser profile: the private half is
//! generated non-extractable and persisted — as an opaque structured
//! clone — in IndexedDB, so key material can never leave the browser
//! through our own code paths. The verified-peer set (safety-number
//! fingerprints we have confirmed out of band) lives beside it.

use crate::e2ee::safety::{PUBLIC_KEY_LEN, fingerprint};
use std::collections::HashSet;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Crypto, CryptoKey, CryptoKeyPair, IdbDatabase, IdbRequest};

const DB_NAME: &str = "huddle-identity";
const DB_VERSION: u32 = 1;
const STORE: &str = "keys";
const IDENTITY_RECORD: &str = "identity";
const VERIFIED_RECORD: &str = "verified";

/// Resolve an IndexedDB request through a oneshot channel: the
/// callbacks are `Closure`s tied to this function's scope, so they
/// drop (and detach) when the await finishes.
async fn await_request(request: IdbRequest) -> Result<JsValue, String> {
    let (tx, rx) = futures::channel::oneshot::channel::<Result<JsValue, String>>();
    let tx = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    let on_success = {
        let tx = tx.clone();
        let request = request.clone();
        Closure::<dyn FnMut()>::new(move || {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(request.result().map_err(|_| "no result".to_owned()));
            }
        })
    };
    let on_error = {
        let tx = tx.clone();
        Closure::<dyn FnMut()>::new(move || {
            if let Some(tx) = tx.borrow_mut().take() {
                let _ = tx.send(Err("indexeddb request failed".to_owned()));
            }
        })
    };
    request.set_onsuccess(Some(
        on_success.as_ref().unchecked_ref::<js_sys::Function>(),
    ));
    request.set_onerror(Some(on_error.as_ref().unchecked_ref::<js_sys::Function>()));
    // The Closures stay alive for the whole await — dropping them any
    // earlier would invalidate the function handles while they are
    // still registered on the request.
    let result = rx
        .await
        .map_err(|_| "indexeddb request dropped".to_owned())?;
    // Detach the callbacks before this frame (and its Closures) drop.
    request.set_onsuccess(None);
    request.set_onerror(None);
    result
}

/// Open (creating on first use) the identity database. The store is
/// created in `onupgradeneeded`, which only ever runs on a fresh
/// create — a second open at the same version does not re-fire it.
async fn open_db() -> Result<IdbDatabase, String> {
    let factory = web_sys::window()
        .ok_or_else(|| "no window".to_owned())?
        .indexed_db()
        .map_err(|_| "indexeddb unavailable".to_owned())?
        .ok_or_else(|| "indexeddb unavailable".to_owned())?;
    let request = factory
        .open_with_u32(DB_NAME, DB_VERSION)
        .map_err(|_| "could not open the identity store".to_owned())?;
    let on_upgrade = {
        let request = request.clone();
        Closure::<dyn FnMut()>::new(move || {
            if let Ok(db) = request.result().and_then(|v| v.dyn_into::<IdbDatabase>()) {
                // Errors if the store already exists — fine.
                let _ = db.create_object_store(STORE);
            }
        })
    };
    request.set_onupgradeneeded(Some(
        on_upgrade.as_ref().unchecked_ref::<js_sys::Function>(),
    ));
    let db = await_request(request.clone().into()).await?;
    // Detach the upgrade callback before its Closure drops with this frame.
    request.set_onupgradeneeded(None);
    drop(on_upgrade);
    db.dyn_into::<IdbDatabase>()
        .map_err(|_| "unexpected identity store shape".to_owned())
}

fn crypto() -> Result<Crypto, String> {
    web_sys::window()
        .ok_or_else(|| "no window".to_owned())?
        .crypto()
        .map_err(|_| "webcrypto unavailable".to_owned())
}

/// Export a public key as its raw 65-byte P-256 point.
async fn export_public(key: &CryptoKey) -> Result<Vec<u8>, String> {
    let raw = JsFuture::from(
        crypto()?
            .subtle()
            .export_key("raw", key)
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?,
    )
    .await
    .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
    Ok(js_sys::Uint8Array::new(&raw).to_vec())
}

/// Generate a fresh identity: ECDH P-256, private half non-extractable,
/// used only for deriveBits.
async fn generate_identity() -> Result<(CryptoKeyPair, Vec<u8>), String> {
    let params = js_sys::Object::new();
    js_sys::Reflect::set(
        &params,
        &JsValue::from_str("name"),
        &JsValue::from_str("ECDH"),
    )
    .map_err(|_| "reflect set failed".to_owned())?;
    js_sys::Reflect::set(
        &params,
        &JsValue::from_str("namedCurve"),
        &JsValue::from_str("P-256"),
    )
    .map_err(|_| "reflect set failed".to_owned())?;
    // The private half is used for deriveBits; the usage must be
    // declared explicitly (Chrome rejects an empty list), and the
    // public half carries no usages of its own.
    let usages = js_sys::Array::new();
    usages.push(&JsValue::from_str("deriveBits"));
    let pair = JsFuture::from(
        crypto()?
            .subtle()
            .generate_key_with_object(&params, false, usages.as_ref())
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?,
    )
    .await
    .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
    let pair = pair.unchecked_into::<CryptoKeyPair>();
    let public = export_public(&CryptoKeyPair::get_public_key(&pair)).await?;
    Ok((pair, public))
}

async fn put_record(db: &IdbDatabase, key: &str, value: &JsValue) -> Result<(), String> {
    let tx = db
        .transaction_with_str_and_mode(STORE, web_sys::IdbTransactionMode::Readwrite)
        .map_err(|_| "identity transaction failed".to_owned())?;
    let store = tx
        .object_store(STORE)
        .map_err(|_| "identity store missing".to_owned())?;
    let request = store
        .put_with_key(value, &JsValue::from_str(key))
        .map_err(|_| "identity write failed".to_owned())?;
    await_request(request).await.map(|_| ())
}

async fn get_record(db: &IdbDatabase, key: &str) -> Result<Option<JsValue>, String> {
    let tx = db
        .transaction_with_str_and_mode(STORE, web_sys::IdbTransactionMode::Readonly)
        .map_err(|_| "identity transaction failed".to_owned())?;
    let store = tx
        .object_store(STORE)
        .map_err(|_| "identity store missing".to_owned())?;
    let request = store
        .get(&JsValue::from_str(key))
        .map_err(|_| "identity read failed".to_owned())?;
    let value = await_request(request).await?;
    if value.is_undefined() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

/// The loaded (or freshly created) browser identity.
pub struct Identity {
    /// The raw 65-byte P-256 public point.
    pub public_bytes: Vec<u8>,
    /// The hex SHA-256 fingerprint of the public point.
    pub fingerprint: String,
    /// The non-extractable private half.
    pub private_key: CryptoKey,
}

/// Load the persisted identity, creating (and storing) one on first
/// use. A corrupt record is regenerated rather than trusted.
pub async fn load_or_create() -> Result<Identity, String> {
    let db = open_db().await?;
    if let Some(existing) = get_record(&db, IDENTITY_RECORD).await? {
        // Same unchecked cast as in `generate_identity` — a corrupt
        // record yields no exportable public key below and falls
        // through to regeneration instead of being trusted.
        let pair = existing.unchecked_into::<CryptoKeyPair>();
        if let Ok(public_bytes) = export_public(&CryptoKeyPair::get_public_key(&pair)).await
            && public_bytes.len() == PUBLIC_KEY_LEN
        {
            return Ok(Identity {
                fingerprint: fingerprint(&public_bytes),
                public_bytes,
                private_key: CryptoKeyPair::get_private_key(&pair),
            });
        }
    }
    let (pair, public_bytes) = generate_identity().await?;
    put_record(&db, IDENTITY_RECORD, pair.as_ref()).await?;
    Ok(Identity {
        fingerprint: fingerprint(&public_bytes),
        public_bytes,
        private_key: CryptoKeyPair::get_private_key(&pair),
    })
}

/// The fingerprints of peers we have verified out of band (ADR 010,
/// D5) — client-side only; the server never learns them.
pub async fn load_verified() -> HashSet<String> {
    let Ok(db) = open_db().await else {
        return HashSet::new();
    };
    let Ok(Some(record)) = get_record(&db, VERIFIED_RECORD).await else {
        return HashSet::new();
    };
    record
        .as_string()
        .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
        .map(|list| list.into_iter().collect())
        .unwrap_or_default()
}

pub async fn store_verified(verified: &HashSet<String>) -> Result<(), String> {
    let db = open_db().await?;
    let list: Vec<String> = verified.iter().cloned().collect();
    let json = serde_json::to_string(&list)
        .map_err(|_| "could not serialize verified peers".to_owned())?;
    put_record(&db, VERIFIED_RECORD, &JsValue::from_str(&json)).await
}
