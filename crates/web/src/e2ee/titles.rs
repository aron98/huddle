//! Client-side meeting-title encryption (issue #29, ADR 012).
//!
//! The organizer's browser seals an E2EE meeting's title to its own
//! long-term identity at creation: a self-ECDH (own private key, own
//! public point) run through the same HKDF/AES-GCM pipeline as the
//! pairwise keys, so only the creating browser profile can open it.
//! The blob rides the opaque `title` column at rest; after joining, the
//! true title reaches the other participants over the E2EE keying
//! relay (tag 0x05, sealed to each pairwise key; 0x04 is the chat
//! envelope's tag per ADR 011 D7).

use crate::e2ee::identity::{self, Identity};
use crate::e2ee::keying;
use base64::Engine;

/// HKDF info for the organizer-scope title key — domain-separated
/// from the pairwise-info derivations in ADR 010.
pub const TITLE_KEY_INFO: &[u8] = b"huddle-titles/v1/organizer-key";
/// AES-GCM additional data binding the stored blob to this format.
pub const TITLE_AAD: &[u8] = b"huddle-titles/v1/title";

/// The organizer-scope title key: HKDF over the self-ECDH secret.
/// Using our own public point as the "peer" key is the standard way to
/// derive a key only we can derive — WebCrypto's ECDH deriveBits is
/// symmetric in the two inputs.
async fn title_key(identity: &Identity) -> Result<web_sys::CryptoKey, String> {
    keying::derive_pairwise_key(
        &identity.private_key,
        &identity.public_bytes,
        TITLE_KEY_INFO,
    )
    .await
}

/// Seal a plaintext title into the `enc1.` storage blob. The caller
/// validates the plaintext first (the server cannot, it never sees it).
pub async fn encrypt_title(plaintext: &str) -> Result<String, String> {
    let identity = identity::load_or_create().await?;
    let key = title_key(&identity).await?;
    let mut nonce = keying::random_bytes(keying::NONCE_LEN)?;
    let ciphertext = keying::seal(&key, &nonce, TITLE_AAD, plaintext.as_bytes()).await?;
    nonce.extend(ciphertext);
    let encoded = base64::engine::general_purpose::STANDARD.encode(&nonce);
    Ok(format!(
        "{}{}",
        huddle_protocol::ENCRYPTED_TITLE_PREFIX,
        encoded
    ))
}

/// Open a stored title. `Ok(None)` for anything without the encrypted
/// marker (plaintext meetings keep their title verbatim); an `Err` for
/// a blob this browser cannot open — another organizer device, or a
/// foreign profile. Callers fall back to the masked label on `Err`.
pub async fn decrypt_title(stored: &str) -> Result<Option<String>, String> {
    let Some(blob) = stored
        .strip_prefix(huddle_protocol::ENCRYPTED_TITLE_PREFIX)
        .map(str::as_bytes)
    else {
        return Ok(None);
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .map_err(|_| "the stored title blob is malformed".to_owned())?;
    if bytes.len() <= keying::NONCE_LEN {
        return Err("the stored title blob is truncated".to_owned());
    }
    let (nonce, ciphertext) = bytes.split_at(keying::NONCE_LEN);
    let identity = identity::load_or_create().await?;
    let key = title_key(&identity).await?;
    let plaintext = keying::open(&key, nonce, TITLE_AAD, ciphertext).await?;
    String::from_utf8(plaintext)
        .map(Some)
        .map_err(|_| "the stored title is not valid UTF-8".to_owned())
}
