//! E2EE chat envelopes (issue #28, ADR 011, D7).
//!
//! Encrypted chat bodies ride the ordinary broadcast chat path: the
//! envelope is an opaque base64 string inside `ChatMessage::body`, and
//! the server constructs, persists, and relays it verbatim — it never
//! inspects bodies. The tag byte continues the keying envelope
//! convention (identity 0x01, sender key 0x02, verified 0x03), with
//! chat taking 0x04.
//!
//! Everything here is pure Rust so the framing and the HKDF context
//! separation are unit-testable natively; the browser half (AES-GCM
//! over these envelopes) lives in [`crate::e2ee::session`].

use crate::e2ee::safety::hkdf_sha256;
use base64::Engine;
use uuid::Uuid;

/// The tag of an encrypted chat envelope.
pub const TAG_CHAT: u8 = 0x04;
/// AES-GCM nonce length (same as the keying envelopes).
pub const NONCE_LEN: usize = 12;
/// Minimum body size: tag + nonce + GCM tag.
pub const MIN_ENVELOPE_LEN: usize = 1 + NONCE_LEN + 16;

/// HKDF context separation: the chat key derives from the sender key
/// with its own info string, so a media key can never be replayed as a
/// chat key and vice versa (ADR 011, D7).
pub const CHAT_KEY_INFO: &[u8] = b"huddle-e2ee/v1/chat";
/// AAD prefix binding a chat envelope to its sender and epoch.
pub const CHAT_AAD_PREFIX: &[u8] = b"huddle-e2ee/v1/chat-message";

/// The AES-128 chat key for one sender key.
pub fn chat_key(sender_key: &[u8]) -> [u8; 16] {
    let key = hkdf_sha256(sender_key, &[], CHAT_KEY_INFO, 16);
    key.try_into().unwrap_or([0; 16])
}

/// The AES-GCM additional data binding a chat body to the claimed
/// sender and the epoch — a relayed envelope from another sender or a
/// stale epoch fails to open.
pub fn chat_aad(sender: Uuid, epoch: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(CHAT_AAD_PREFIX.len() + 16 + 8);
    aad.extend_from_slice(CHAT_AAD_PREFIX);
    aad.extend_from_slice(sender.as_bytes());
    aad.extend_from_slice(&epoch.to_le_bytes());
    aad
}

/// Encode an encrypted chat envelope: `base64(0x04 ‖ nonce ‖ ct+tag)`.
pub fn encode(nonce: &[u8], ciphertext: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(1 + nonce.len() + ciphertext.len());
    bytes.push(TAG_CHAT);
    bytes.extend_from_slice(nonce);
    bytes.extend_from_slice(ciphertext);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode an encrypted chat envelope; `None` on anything that is not a
/// well-formed 0x04 envelope (including ordinary plaintext chat).
pub fn decode(payload: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    if bytes.len() < MIN_ENVELOPE_LEN || bytes[0] != TAG_CHAT {
        return None;
    }
    let nonce = bytes[1..1 + NONCE_LEN].to_vec();
    let ciphertext = bytes[1 + NONCE_LEN..].to_vec();
    Some((nonce, ciphertext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_envelopes_round_trip() {
        let nonce = vec![9u8; NONCE_LEN];
        let ciphertext = vec![7u8; 32];
        let envelope = encode(&nonce, &ciphertext);
        assert_eq!(decode(&envelope), Some((nonce, ciphertext)));
    }

    #[test]
    fn plaintext_and_malformed_bodies_are_not_envelopes() {
        assert_eq!(decode("hello world"), None);
        assert_eq!(decode(""), None);
        // Valid base64, wrong tag (a keying envelope).
        let keying = base64::engine::general_purpose::STANDARD.encode([0x02u8, 1, 2, 3]);
        assert_eq!(decode(&keying), None);
        // Right tag but too short to hold a nonce plus GCM tag.
        let mut short_bytes = vec![TAG_CHAT];
        short_bytes.extend_from_slice(&[0u8; NONCE_LEN + 8]);
        let short = base64::engine::general_purpose::STANDARD.encode(short_bytes);
        assert_eq!(decode(&short), None);
    }

    #[test]
    fn chat_keys_are_separated_from_sender_keys() {
        let sender_key = [0x11u8; 16];
        let key = chat_key(&sender_key);
        assert_eq!(key.len(), 16);
        assert_ne!(key.to_vec(), sender_key.to_vec());
        // The info string is the separator: same input, other context.
        let other: [u8; 16] =
            crate::e2ee::safety::hkdf_sha256(&sender_key, &[], b"huddle-e2ee/v1/other", 16)
                .try_into()
                .unwrap();
        assert_ne!(key, other);
    }

    #[test]
    fn chat_aad_binds_sender_and_epoch() {
        let a = Uuid::nil();
        let b = Uuid::max();
        assert_eq!(chat_aad(a, 5), chat_aad(a, 5));
        assert_ne!(chat_aad(a, 5), chat_aad(b, 5));
        assert_ne!(chat_aad(a, 5), chat_aad(a, 6));
        let aad = chat_aad(a, 5);
        assert_eq!(
            aad,
            [CHAT_AAD_PREFIX, a.as_bytes(), &5u64.to_le_bytes()].concat()
        );
    }
}
