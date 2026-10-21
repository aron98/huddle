//! Keying-envelope framing for the E2EE relay (ADR 010, D4).
//!
//! Envelopes are `base64(tag ‖ data)` strings carried by the dedicated
//! `e2ee_keying` signaling messages; the server relays them without
//! ever seeing usable key material. The tag byte keeps the format
//! extensible (new kinds of payloads — e.g. #28's encrypted chat —
//! ride the same relay without a protocol change).

use crate::e2ee::safety::{PUBLIC_KEY_LEN, SENDER_KEY_AAD_PREFIX, VERIFIED_AAD_PREFIX};
use base64::Engine;
use uuid::Uuid;

/// An announced long-term identity: the raw 65-byte P-256 point.
pub const TAG_IDENTITY: u8 = 0x01;
/// A sender key sealed to the receiver's pairwise key.
pub const TAG_SENDER_KEY: u8 = 0x02;
/// A verification confirmation sealed to the receiver's pairwise key.
pub const TAG_VERIFIED: u8 = 0x03;

/// AES-GCM nonce length (the standard 96-bit size).
pub const NONCE_LEN: usize = 12;
/// SFrame base-key length (AES-128).
pub const SENDER_KEY_LEN: usize = 16;

/// One decoded keying envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyingPayload {
    Identity { public: Vec<u8> },
    SenderKey { nonce: Vec<u8>, ciphertext: Vec<u8> },
    Verified { nonce: Vec<u8>, ciphertext: Vec<u8> },
}

/// Encode an identity announce (ADR 010, D4 tag 0x01).
pub fn encode_identity(public: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(1 + public.len());
    bytes.push(TAG_IDENTITY);
    bytes.extend_from_slice(public);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Encode a sealed (AES-GCM) envelope: `tag ‖ nonce ‖ ciphertext`,
/// where the ciphertext already carries the GCM tag.
fn encode_sealed(tag: u8, nonce: &[u8], ciphertext: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(1 + nonce.len() + ciphertext.len());
    bytes.push(tag);
    bytes.extend_from_slice(nonce);
    bytes.extend_from_slice(ciphertext);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Encode a sender-key envelope (ADR 010, D4 tag 0x02).
pub fn encode_sender_key(nonce: &[u8], ciphertext: &[u8]) -> String {
    encode_sealed(TAG_SENDER_KEY, nonce, ciphertext)
}

/// Encode a verification-confirmation envelope (ADR 010, D4 tag 0x03).
pub fn encode_verified(nonce: &[u8], ciphertext: &[u8]) -> String {
    encode_sealed(TAG_VERIFIED, nonce, ciphertext)
}

/// Decode an envelope; `None` on any malformed input.
pub fn decode(payload: &str) -> Option<KeyingPayload> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    let (tag, data) = bytes.split_first()?;
    match *tag {
        TAG_IDENTITY if data.len() == PUBLIC_KEY_LEN => Some(KeyingPayload::Identity {
            public: data.to_vec(),
        }),
        TAG_SENDER_KEY if data.len() > NONCE_LEN => Some(KeyingPayload::SenderKey {
            nonce: data[..NONCE_LEN].to_vec(),
            ciphertext: data[NONCE_LEN..].to_vec(),
        }),
        TAG_VERIFIED if data.len() > NONCE_LEN => Some(KeyingPayload::Verified {
            nonce: data[..NONCE_LEN].to_vec(),
            ciphertext: data[NONCE_LEN..].to_vec(),
        }),
        _ => None,
    }
}

/// The AES-GCM additional data binding a sender key to its sender and
/// receiver — a relayed envelope is useless to anyone else.
pub fn sender_key_aad(sender: Uuid, receiver: Uuid) -> Vec<u8> {
    let mut aad = Vec::with_capacity(SENDER_KEY_AAD_PREFIX.len() + 32);
    aad.extend_from_slice(SENDER_KEY_AAD_PREFIX);
    aad.extend_from_slice(sender.as_bytes());
    aad.extend_from_slice(receiver.as_bytes());
    aad
}

/// The additional data binding a verification confirmation to its
/// sender and receiver.
pub fn verified_aad(sender: Uuid, receiver: Uuid) -> Vec<u8> {
    let mut aad = Vec::with_capacity(VERIFIED_AAD_PREFIX.len() + 32);
    aad.extend_from_slice(VERIFIED_AAD_PREFIX);
    aad.extend_from_slice(sender.as_bytes());
    aad.extend_from_slice(receiver.as_bytes());
    aad
}

/// The sender-key plaintext: `epoch_le64 ‖ key` — the epoch is bound
/// *inside* the ciphertext so a relayed old-epoch key cannot be
/// replayed into a new epoch (ADR 010, D4).
pub fn sender_key_plaintext(epoch: u64, key: &[u8; SENDER_KEY_LEN]) -> Vec<u8> {
    let mut plaintext = Vec::with_capacity(8 + SENDER_KEY_LEN);
    plaintext.extend_from_slice(&epoch.to_le_bytes());
    plaintext.extend_from_slice(key);
    plaintext
}

/// Inverse of [`sender_key_plaintext`]; `None` on a wrong length.
pub fn decode_sender_key_plaintext(plaintext: &[u8]) -> Option<(u64, [u8; SENDER_KEY_LEN])> {
    if plaintext.len() != 8 + SENDER_KEY_LEN {
        return None;
    }
    let epoch = u64::from_le_bytes(plaintext[..8].try_into().ok()?);
    let key = plaintext[8..].try_into().ok()?;
    Some((epoch, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_envelopes_round_trip() {
        let public = vec![0x04u8; PUBLIC_KEY_LEN];
        let encoded = encode_identity(&public);
        assert_eq!(
            decode(&encoded),
            Some(KeyingPayload::Identity {
                public: public.clone()
            })
        );
    }

    #[test]
    fn sealed_envelopes_round_trip_for_both_tags() {
        let nonce = vec![7u8; NONCE_LEN];
        let ciphertext = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let sender = encode_sender_key(&nonce, &ciphertext);
        assert_eq!(
            decode(&sender),
            Some(KeyingPayload::SenderKey {
                nonce: nonce.clone(),
                ciphertext: ciphertext.clone()
            })
        );
        let verified = encode_verified(&nonce, &ciphertext);
        assert_eq!(
            decode(&verified),
            Some(KeyingPayload::Verified { nonce, ciphertext })
        );
        // The tags must not be confusable.
        assert_ne!(sender, verified);
    }

    #[test]
    fn malformed_envelopes_are_rejected() {
        assert_eq!(decode("not base64!!"), None);
        assert_eq!(
            decode(&base64::engine::general_purpose::STANDARD.encode([0x7f, 1])),
            None
        );
        // Identity with a wrong public-key length.
        let short_identity =
            base64::engine::general_purpose::STANDARD.encode([TAG_IDENTITY, 0x04, 0x02]);
        assert_eq!(decode(&short_identity), None);
        // Sealed envelopes need a nonce plus at least the GCM tag.
        let mut empty_sealed = vec![TAG_SENDER_KEY];
        empty_sealed.extend_from_slice(&[0u8; NONCE_LEN]);
        let empty_sealed = base64::engine::general_purpose::STANDARD.encode(empty_sealed);
        assert_eq!(decode(&empty_sealed), None);
    }

    #[test]
    fn sender_key_plaintext_round_trips() {
        let key = [0xabu8; SENDER_KEY_LEN];
        let plaintext = sender_key_plaintext(42, &key);
        assert_eq!(decode_sender_key_plaintext(&plaintext), Some((42, key)));
        assert_eq!(
            decode_sender_key_plaintext(&plaintext[..plaintext.len() - 1]),
            None
        );
        assert_eq!(decode_sender_key_plaintext(&[]), None);
    }

    #[test]
    fn aads_bind_their_participants() {
        let sender = Uuid::nil();
        let receiver = Uuid::max();
        let aad = sender_key_aad(sender, receiver);
        assert_eq!(
            aad,
            [
                SENDER_KEY_AAD_PREFIX,
                sender.as_bytes(),
                receiver.as_bytes()
            ]
            .concat()
        );
        assert_ne!(aad, verified_aad(sender, receiver));
        // Swapping roles yields a different AAD — a confirmation meant
        // for one direction cannot be replayed in the other.
        assert_ne!(
            sender_key_aad(sender, receiver),
            sender_key_aad(receiver, sender)
        );
    }
}

/// The WebCrypto-backed half (browser only): pairwise-key derivation,
/// AES-GCM sealing, and randomness. The native build sees only the
/// framing above, so the crypto stays unit-testable where it matters
/// (the framing and derivation inputs) and exercised by e2e in the
/// browser.
#[cfg(target_arch = "wasm32")]
mod crypto {
    use super::super::safety;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{CryptoKey, SubtleCrypto};

    fn subtle() -> Result<SubtleCrypto, String> {
        web_sys::window()
            .ok_or_else(|| "no window".to_owned())?
            .crypto()
            .map_err(|_| "webcrypto unavailable".to_owned())
            .map(|crypto| crypto.subtle())
    }

    fn js_err(value: JsValue) -> String {
        value.as_string().unwrap_or_else(|| format!("{value:?}"))
    }

    fn set(obj: &js_sys::Object, key: &str, value: &JsValue) -> Result<(), String> {
        js_sys::Reflect::set(obj, &JsValue::from_str(key), value)
            .map(|_| ())
            .map_err(|_| "reflect set failed".to_owned())
    }

    fn gcm_params(nonce: &[u8], aad: &[u8]) -> Result<js_sys::Object, String> {
        let params = js_sys::Object::new();
        set(&params, "name", &JsValue::from_str("AES-GCM"))?;
        set(&params, "iv", js_sys::Uint8Array::from(nonce).as_ref())?;
        set(
            &params,
            "additionalData",
            js_sys::Uint8Array::from(aad).as_ref(),
        )?;
        set(&params, "tagLength", &JsValue::from_f64(128.0))?;
        Ok(params)
    }

    async fn import_ecdh_public(raw: &[u8]) -> Result<CryptoKey, String> {
        let params = js_sys::Object::new();
        set(&params, "name", &JsValue::from_str("ECDH"))?;
        set(&params, "namedCurve", &JsValue::from_str("P-256"))?;
        // The public half of an ECDH pair is used only as an input to
        // deriveBits — no key-usages entries.
        let usages = js_sys::Array::new();
        let buffer = js_sys::Uint8Array::from(raw).buffer();
        let key = JsFuture::from(
            subtle()?
                .import_key_with_object(
                    "raw",
                    buffer.unchecked_ref::<js_sys::Object>(),
                    &params,
                    false,
                    usages.as_ref(),
                )
                .map_err(js_err)?,
        )
        .await
        .map_err(js_err)?;
        // `CryptoKey` has no JS constructor, so the instanceof-based
        // cast never succeeds — the value comes straight from the
        // WebCrypto promise; the next WebCrypto call validates it.
        Ok(key.unchecked_into::<CryptoKey>())
    }

    /// Import raw bytes as an AES-GCM `CryptoKey` (used for pairwise
    /// keys and for #28's chat keys).
    pub async fn import_aes_key(raw: &[u8]) -> Result<CryptoKey, String> {
        let params = js_sys::Object::new();
        set(&params, "name", &JsValue::from_str("AES-GCM"))?;
        let usages = js_sys::Array::new();
        usages.push(&JsValue::from_str("encrypt"));
        usages.push(&JsValue::from_str("decrypt"));
        let buffer = js_sys::Uint8Array::from(raw).buffer();
        let key = JsFuture::from(
            subtle()?
                .import_key_with_object(
                    "raw",
                    buffer.unchecked_ref::<js_sys::Object>(),
                    &params,
                    false,
                    usages.as_ref(),
                )
                .map_err(js_err)?,
        )
        .await
        .map_err(js_err)?;
        // `CryptoKey` has no JS constructor, so the instanceof-based
        // cast never succeeds — the value comes straight from the
        // WebCrypto promise; the next WebCrypto call validates it.
        Ok(key.unchecked_into::<CryptoKey>())
    }

    /// Derive the AES-128-GCM pairwise key from our private key and
    /// the peer's raw public key (ADR 010, D2): ECDH → HKDF-SHA256.
    pub async fn derive_pairwise_key(
        our_private: &CryptoKey,
        their_public: &[u8],
        info: &[u8],
    ) -> Result<CryptoKey, String> {
        let their_public_key = import_ecdh_public(their_public).await?;
        let params = js_sys::Object::new();
        set(&params, "name", &JsValue::from_str("ECDH"))?;
        set(&params, "public", their_public_key.as_ref())?;
        let bits = JsFuture::from(
            subtle()?
                .derive_bits_with_object(&params, our_private, 256)
                .map_err(js_err)?,
        )
        .await
        .map_err(js_err)?;
        let secret = js_sys::Uint8Array::new(&bits).to_vec();
        let pairwise = safety::hkdf_sha256(&secret, &[], info, safety::PAIRWISE_KEY_LEN);
        import_aes_key(&pairwise).await
    }

    /// AES-GCM seal; the 128-bit tag is appended to the ciphertext.
    pub async fn seal(
        key: &CryptoKey,
        nonce: &[u8],
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, String> {
        let params = gcm_params(nonce, aad)?;
        let sealed = JsFuture::from(
            subtle()?
                .encrypt_with_object_and_u8_array(&params, key, plaintext)
                .map_err(js_err)?,
        )
        .await
        .map_err(js_err)?;
        Ok(js_sys::Uint8Array::new(&sealed).to_vec())
    }

    /// AES-GCM open; a wrong key, nonce, or AAD fails the promise.
    pub async fn open(
        key: &CryptoKey,
        nonce: &[u8],
        aad: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, String> {
        let params = gcm_params(nonce, aad)?;
        let plaintext = JsFuture::from(
            subtle()?
                .decrypt_with_object_and_u8_array(&params, key, ciphertext)
                .map_err(js_err)?,
        )
        .await
        .map_err(js_err)?;
        Ok(js_sys::Uint8Array::new(&plaintext).to_vec())
    }

    /// Cryptographically secure random bytes via WebCrypto.
    pub fn random_bytes(len: usize) -> Result<Vec<u8>, String> {
        let mut bytes = vec![0u8; len];
        web_sys::window()
            .ok_or_else(|| "no window".to_owned())?
            .crypto()
            .map_err(|_| "webcrypto unavailable".to_owned())?
            .get_random_values_with_u8_array(&mut bytes)
            .map_err(js_err)?;
        Ok(bytes)
    }
}

#[cfg(target_arch = "wasm32")]
pub use crypto::{derive_pairwise_key, import_aes_key, open, random_bytes, seal};
