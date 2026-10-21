//! SFrame reference framing (RFC 9605, issue #28 / ADR 011, D2+D3).
//!
//! The per-frame work at runtime happens in the vendored worker JS
//! (`assets/e2ee/e2ee-worker.js`), which mirrors this module byte for byte.
//! What lives here is everything testable natively: the HKDF key/salt
//! derivation of RFC 9605 §4.4.2 (pinned against the published test
//! vectors), the fixed header form, and the nonce construction.

use crate::e2ee::safety::hkdf_sha256;

/// `AES_128_GCM_SHA256_128` (RFC 9605 §4.2, Table 2).
pub const CIPHER_SUITE: u16 = 0x0004;
/// HKDF label prefix for the per-KID key (RFC 9605 §4.4.2).
pub const KEY_LABEL: &[u8] = b"SFrame 1.0 Secret key ";
/// HKDF label prefix for the per-KID salt (RFC 9605 §4.4.2).
pub const SALT_LABEL: &[u8] = b"SFrame 1.0 Secret salt ";
/// AES-128 key length.
pub const KEY_LEN: usize = 16;
/// GCM nonce length.
pub const SALT_LEN: usize = 12;
/// Fixed header form: `0xBE ‖ KID_be32 ‖ CTR_be56` (ADR 011, D2). The
/// config byte says "extended KID of 4 bytes, extended CTR of 7 bytes".
pub const HEADER_LEN: usize = 12;
pub const HEADER_CONFIG: u8 = 0xBE;
/// AES-GCM tag length.
pub const TAG_LEN: usize = 16;

/// The per-KID key and salt of RFC 9605 §4.4.2: HKDF-Expand of the
/// extracted base key with the label, the KID (8 bytes big-endian) and
/// the cipher suite (2 bytes big-endian) as info.
pub fn key_salt(base_key: &[u8], kid: u32) -> ([u8; KEY_LEN], [u8; SALT_LEN]) {
    let key = hkdf_expand_labeled(base_key, KEY_LABEL, kid, KEY_LEN);
    let salt = hkdf_expand_labeled(base_key, SALT_LABEL, kid, SALT_LEN);
    (
        key.try_into().unwrap_or([0; KEY_LEN]),
        salt.try_into().unwrap_or([0; SALT_LEN]),
    )
}

fn hkdf_expand_labeled(base_key: &[u8], label: &[u8], kid: u32, len: usize) -> Vec<u8> {
    let mut info = Vec::with_capacity(label.len() + 8 + 2);
    info.extend_from_slice(label);
    // The KID is 8 bytes big-endian, and the cipher suite value
    // follows it (RFC 9605 §4.4.2).
    info.extend_from_slice(&u64::from(kid).to_be_bytes());
    info.extend_from_slice(&CIPHER_SUITE.to_be_bytes());
    // hkdf_sha256 extracts with an empty salt first, exactly the
    // RFC's `HKDF-Extract("", base_key)`.
    hkdf_sha256(base_key, &[], &info, len)
}

/// Encode the fixed header form: config byte, 4-byte big-endian KID,
/// 7-byte big-endian CTR.
pub fn header(kid: u32, ctr: u64) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[0] = HEADER_CONFIG;
    header[1..5].copy_from_slice(&kid.to_be_bytes());
    let ctr_bytes = ctr.to_be_bytes();
    header[5..12].copy_from_slice(&ctr_bytes[1..]);
    header
}

/// Parse the fixed header form; `None` on a wrong config byte or length.
/// Returns the KID, the CTR, and the AAD (the header bytes themselves).
pub fn parse_header(bytes: &[u8]) -> Option<(u32, u64, [u8; HEADER_LEN])> {
    if bytes.len() < HEADER_LEN || bytes[0] != HEADER_CONFIG {
        return None;
    }
    let kid = u32::from_be_bytes(bytes[1..5].try_into().ok()?);
    let mut ctr_bytes = [0u8; 8];
    ctr_bytes[1..].copy_from_slice(&bytes[5..12]);
    let ctr = u64::from_be_bytes(ctr_bytes);
    Some((kid, ctr, bytes[..HEADER_LEN].try_into().ok()?))
}

/// The GCM nonce: the salt XOR the CTR as a 12-byte big-endian value
/// (RFC 9605 §4.4.3). In the fixed header form the CTR occupies bytes
/// 5..12, so the zero-extension means only those bytes are XORed in.
pub fn nonce(salt: &[u8; SALT_LEN], ctr: u64) -> [u8; SALT_LEN] {
    let header = header(0, ctr);
    let mut nonce = *salt;
    for i in 5..SALT_LEN {
        nonce[i] ^= header[i];
    }
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 9605 AES_128_GCM_SHA256_128 test vector (the published
    /// sframe-wg vector set, commit 025d568): KID 291, CTR 17767.
    #[test]
    fn key_salt_matches_the_rfc_vector() {
        let base_key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let (key, salt) = key_salt(&base_key, 291);
        assert_eq!(
            key,
            [
                0xd3, 0x4f, 0x54, 0x7f, 0x4c, 0xa4, 0xf9, 0xa7, 0x44, 0x70, 0x06, 0xfe, 0x7f, 0xcb,
                0xf7, 0x68
            ]
        );
        assert_eq!(
            salt,
            [
                0x75, 0x23, 0x4e, 0xde, 0xfe, 0x07, 0x81, 0x90, 0x26, 0x75, 0x18, 0x16
            ]
        );
    }

    /// Same vector, nonce construction: salt XOR CTR (17767 = 0x4567)
    /// gives the RFC's `75234edefe07819026755d71`.
    #[test]
    fn nonce_matches_the_rfc_vector() {
        let salt = [
            0x75, 0x23, 0x4e, 0xde, 0xfe, 0x07, 0x81, 0x90, 0x26, 0x75, 0x18, 0x16,
        ];
        assert_eq!(
            nonce(&salt, 17767),
            [
                0x75, 0x23, 0x4e, 0xde, 0xfe, 0x07, 0x81, 0x90, 0x26, 0x75, 0x5d, 0x71
            ]
        );
    }

    #[test]
    fn headers_round_trip_and_reject_garbage() {
        let frame = header(0xdead_beef, 0x0123_4567_89ab);
        assert_eq!(frame[0], HEADER_CONFIG);
        assert_eq!(frame[1..5], 0xdead_beefu32.to_be_bytes());
        let (kid, ctr, aad) = parse_header(&frame).expect("parses");
        assert_eq!(kid, 0xdead_beef);
        assert_eq!(ctr, 0x0123_4567_89ab);
        assert_eq!(aad, frame);
        // The AAD is the whole header, exactly as the worker frames it.
        assert_eq!(parse_header(&frame[..11]), None);
        assert_eq!(parse_header(&[0u8; HEADER_LEN]), None);
        // The CTR field carries 56 bits.
        assert_eq!(
            parse_header(&header(u32::MAX, u64::from(0xffff_ffffu32)))
                .unwrap()
                .1,
            0xffff_ffff
        );
    }

    #[test]
    fn nonces_advance_and_never_repeat() {
        let salt = [0xaau8; SALT_LEN];
        let first = nonce(&salt, 0);
        let second = nonce(&salt, 1);
        let late = nonce(&salt, 1 << 40);
        assert_ne!(first, second);
        assert_ne!(second, late);
        // XOR with the salt: the counter only ever touches bytes 5..12.
        assert_eq!(first[..5], salt[..5]);
        assert_eq!(late[0], salt[0]);
    }
}
