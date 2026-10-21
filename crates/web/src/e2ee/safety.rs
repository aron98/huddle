//! Safety numbers, fingerprints and HKDF — the pure-Rust heart of the
//! E2EE design (ADR 010, D2 + D5). Everything here is natively
//! testable; the browser side only supplies raw ECDH secrets.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Domain separation: every derivation binds its own tag so the output
/// of one can never be replayed as an input to another (ADR 010, D5).
pub const SAFETY_NUMBER_INFO: &[u8] = b"huddle-e2ee/v1/safety-number";
pub const PAIRWISE_INFO: &[u8] = b"huddle-e2ee/v1/pairwise";
pub const KID_INFO: &[u8] = b"huddle-e2ee/v1/kid";
pub const SENDER_KEY_AAD_PREFIX: &[u8] = b"huddle-e2ee/v1/sender-key";
pub const VERIFIED_AAD_PREFIX: &[u8] = b"huddle-e2ee/v1/verified";
/// Binds an in-call title envelope (issue #29, tag 0x04) to its sender
/// and receiver — same pattern as the sender-key and verified AADs.
pub const TITLE_AAD_PREFIX: &[u8] = b"huddle-e2ee/v1/title";

/// Raw ECDH P-256 public keys on the wire: uncompressed points.
pub const PUBLIC_KEY_LEN: usize = 65;

/// The pairwise AES-128-GCM key length (AES-128).
pub const PAIRWISE_KEY_LEN: usize = 16;

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts keys of any length, so this cannot fail (same
    // pattern as the server's cookie signing).
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
        Err(_) => unreachable!("hmac accepts arbitrary key lengths"),
    }
}

/// HKDF-SHA256 (RFC 5869): extract-then-expand. An absent salt is a
/// hash-length block of zeros per the RFC. Implemented here in pure
/// Rust (sha2 + hmac) rather than WebCrypto's HKDF so the construction
/// is unit-testable against the RFC vectors natively — see ADR 010 D7.
pub fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let salt = if salt.is_empty() {
        vec![0u8; 32]
    } else {
        salt.to_vec()
    };
    let prk = hmac_sha256(&salt, ikm);
    let mut okm = Vec::with_capacity(len);
    let mut previous: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    while okm.len() < len {
        let mut block_input = Vec::with_capacity(previous.len() + info.len() + 1);
        block_input.extend_from_slice(&previous);
        block_input.extend_from_slice(info);
        block_input.push(counter);
        previous = hmac_sha256(&prk, &block_input);
        okm.extend_from_slice(&previous);
        counter = counter.saturating_add(1);
    }
    okm.truncate(len);
    okm
}

/// The hex SHA-256 fingerprint of a raw public key (the stable handle
/// under which verified peers are recorded).
pub fn fingerprint(public: &[u8]) -> String {
    Sha256::digest(public)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// HKDF info for a pairwise key: both fingerprints in sorted order, so
/// both endpoints derive identical info without any negotiation.
pub fn pairwise_info(fp_a: &str, fp_b: &str) -> Vec<u8> {
    let (low, high) = if fp_a <= fp_b {
        (fp_a, fp_b)
    } else {
        (fp_b, fp_a)
    };
    let mut info = PAIRWISE_INFO.to_vec();
    info.extend_from_slice(low.as_bytes());
    info.extend_from_slice(high.as_bytes());
    info
}

/// The safety number for a pair of raw public keys: 12 digits derived
/// from the sorted keys (symmetric — both endpoints compute the same
/// number; ADR 010, D5). Identity-bound, so it persists across
/// meetings with the same person.
pub fn safety_number(public_a: &[u8], public_b: &[u8]) -> String {
    let (low, high) = if public_a <= public_b {
        (public_a, public_b)
    } else {
        (public_b, public_a)
    };
    let mut input = SAFETY_NUMBER_INFO.to_vec();
    input.extend_from_slice(low);
    input.extend_from_slice(high);
    let digest = Sha256::digest(&input);
    // The first 40 bits, big-endian: a u64 read of the first 8 bytes
    // with the low 24 bits shifted out.
    let value = u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8])) >> 24;
    format!("{:012}", value % 1_000_000_000_000)
}

/// Display form: four groups of three digits ("123 456 789 012").
pub fn format_safety_number(digits: &str) -> String {
    digits
        .as_bytes()
        .chunks(3)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The SFrame KID for a sender in an epoch (consumed by #28's transform
/// worker to key its outgoing frames).
pub fn kid(epoch: u64, sender: Uuid) -> u32 {
    let mut input = KID_INFO.to_vec();
    input.extend_from_slice(&epoch.to_le_bytes());
    input.extend_from_slice(sender.as_bytes());
    let digest = Sha256::digest(&input);
    u32::from_le_bytes(digest[..4].try_into().unwrap_or([0; 4]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A syntactically valid uncompressed P-256 point with varying
    /// content (the first byte is the 0x04 "uncompressed" marker).
    fn key(seed: u8) -> [u8; PUBLIC_KEY_LEN] {
        let mut key = [0u8; PUBLIC_KEY_LEN];
        key[0] = 0x04;
        key[1] = seed;
        key
    }

    /// RFC 5869 test case 1 (SHA-256).
    #[test]
    fn hkdf_matches_rfc5869_test_case_1() {
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0x00u8..=0x0c).collect();
        let info: Vec<u8> = (0xf0u8..=0xf9).collect();
        let okm = hkdf_sha256(&ikm, &salt, &info, 42);
        assert_eq!(
            okm,
            [
                0x3c, 0xb2, 0x5f, 0x25, 0xfa, 0xac, 0xd5, 0x7a, 0x90, 0x43, 0x4f, 0x64, 0xd0, 0x36,
                0x2f, 0x2a, 0x2d, 0x2d, 0x0a, 0x90, 0xcf, 0x1a, 0x5a, 0x4c, 0x5d, 0xb0, 0x2d, 0x56,
                0xec, 0xc4, 0xc5, 0xbf, 0x34, 0x00, 0x72, 0x08, 0xd5, 0xb8, 0x87, 0x18, 0x58, 0x65,
            ]
        );
    }

    #[test]
    fn hkdf_empty_salt_matches_zero_salt() {
        let ikm = key(1);
        assert_eq!(
            hkdf_sha256(&ikm, &[], b"info", 32),
            hkdf_sha256(&ikm, &[0u8; 32], b"info", 32)
        );
    }

    #[test]
    fn safety_numbers_are_symmetric_stable_and_numeric() {
        let a = key(1);
        let b = key(2);
        assert_eq!(safety_number(&a, &b), safety_number(&b, &a));
        assert_eq!(safety_number(&a, &b), safety_number(&a, &b));
        let number = safety_number(&a, &b);
        assert_eq!(number.len(), 12);
        assert!(number.bytes().all(|c| c.is_ascii_digit()));
        assert_ne!(number, safety_number(&a, &key(3)));
    }

    #[test]
    fn format_groups_digits_in_triples() {
        assert_eq!(format_safety_number("123456789012"), "123 456 789 012");
        assert_eq!(format_safety_number("000000000001"), "000 000 000 001");
    }

    #[test]
    fn fingerprints_are_hex_and_unique() {
        assert_eq!(fingerprint(&key(1)).len(), 64);
        assert_eq!(fingerprint(&key(1)), fingerprint(&key(1)));
        assert_ne!(fingerprint(&key(1)), fingerprint(&key(2)));
    }

    #[test]
    fn pairwise_info_is_symmetric_and_binding() {
        assert_eq!(pairwise_info("aa", "zz"), pairwise_info("zz", "aa"));
        assert_ne!(pairwise_info("aa", "zz"), pairwise_info("aa", "yy"));
    }

    #[test]
    fn kids_track_sender_and_epoch() {
        assert_ne!(kid(1, Uuid::nil()), kid(2, Uuid::nil()));
        assert_ne!(kid(1, Uuid::nil()), kid(1, Uuid::max()));
        assert_eq!(kid(1, Uuid::nil()), kid(1, Uuid::nil()));
    }
}
