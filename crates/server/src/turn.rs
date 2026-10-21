//! TURN REST API credentials (issue #59).
//!
//! When coturn runs with `use-auth-secret`, it only accepts "long-term"
//! credentials whose password is the HMAC-SHA1 of the username under a
//! shared secret, with the username itself carrying the password's
//! expiry as a UNIX timestamp. huddle mints such a pair at meeting
//! join and advertises it on every `turn:`/`turns:` ICE server,
//! replacing any static `[[ice_servers]]` username/credential.
//!
//! Algorithm (draft-uberti-behave-turn-rest-00 §3, "TURN REST API";
//! see also coturn's wiki/README "TURN REST API" section):
//!
//! ```text
//! username   = <expiry, seconds since the UNIX epoch>
//! credential = base64(HMAC-SHA1(shared-secret, username))
//! ```
//!
//! The draft's username form is `<expiry>:<user>`, but coturn accepts
//! the bare timestamp too (with no `--rest-api-separator` in the
//! username it treats the whole string as the expiry) — the same shape
//! Pion and mediasoup ship — so huddle keeps it simple: plain expiry,
//! no user suffix.
//!
//! coturn checks the timestamp only when a client requests a *new*
//! allocation: an established relay session outlives the expiry, so a
//! lapsed credential costs only reconnects, which re-join and get a
//! fresh pair.

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use time::{Duration, OffsetDateTime};

/// The validated `[turn]` selection: everything the engine needs to
/// mint join-time credentials. Built by [`crate::config::Config::turn_selection`].
#[derive(Debug, Clone)]
pub(crate) struct TurnRestAuth {
    /// Mirrored from coturn's `static-auth-secret`.
    pub(crate) shared_secret: String,
    /// Minutes after minting that a credential stays usable for new
    /// TURN allocations.
    pub(crate) credential_ttl_min: u32,
}

impl TurnRestAuth {
    /// Mint a credential pair expiring `credential_ttl_min` from now.
    pub(crate) fn mint(&self) -> (String, String) {
        let expiry =
            OffsetDateTime::now_utc() + Duration::minutes(i64::from(self.credential_ttl_min));
        generate(&self.shared_secret, expiry)
    }
}

/// Generate the `(username, credential)` pair for `expiry`. Pure, so
/// tests can pin exact outputs and cross-check them against the
/// openssl one-liner from the deploy guides.
pub(crate) fn generate(shared_secret: &str, expiry: OffsetDateTime) -> (String, String) {
    let username = expiry.unix_timestamp().to_string();
    let mut mac = match Hmac::<Sha1>::new_from_slice(shared_secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => unreachable!("hmac accepts arbitrary key lengths"),
    };
    mac.update(username.as_bytes());
    let credential = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    (username, credential)
}

/// Whether `url` addresses a TURN relay (`turn:`/`turns:` scheme) —
/// the servers whose advertised credentials REST auth replaces.
/// STUN servers need no credentials and are left untouched.
pub(crate) fn is_turn_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("turn:") || url.starts_with("turns:")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground truth from the openssl one-liner the deploy guides
    /// recommend for manual verification:
    /// `printf %s <username> | openssl dgst -sha1 -hmac <secret> -binary | base64`
    #[test]
    fn known_vectors_match_openssl() {
        let expiry = OffsetDateTime::from_unix_timestamp(1_740_000_000).unwrap();
        let (username, credential) = generate("huddle-test-secret", expiry);
        assert_eq!(username, "1740000000");
        assert_eq!(credential, "2yMyDGNB/zf9ok0ucgmY9GTk0ug=");

        let (username, credential) = generate("another-shared-secret", expiry);
        assert_eq!(username, "1740000000");
        assert_eq!(credential, "wub0B+m5MX+1We5TsGQXGBoC9/4=");

        let expiry = OffsetDateTime::from_unix_timestamp(1_735_689_600).unwrap();
        let (username, credential) = generate("huddle-test-secret", expiry);
        assert_eq!(username, "1735689600");
        assert_eq!(credential, "X4Kv9g5VPY0Rxwg++wHWxskJx38=");
    }

    #[test]
    fn credentials_rotate_with_expiry() {
        let base = OffsetDateTime::from_unix_timestamp(1_740_000_000).unwrap();
        let (user_a, cred_a) = generate("secret", base);
        let (user_b, cred_b) = generate("secret", base + Duration::minutes(1));
        assert_ne!(user_a, user_b);
        assert_ne!(cred_a, cred_b);
    }

    #[test]
    fn minted_pair_is_hmac_valid_and_expires_within_ttl() {
        let auth = TurnRestAuth {
            shared_secret: "huddle-test-secret".into(),
            credential_ttl_min: 30,
        };
        let before = OffsetDateTime::now_utc();
        let (username, credential) = auth.mint();
        let expiry: i64 = username.parse().unwrap();
        // Time-bounded: not already expired, not valid past the TTL.
        assert!(expiry > before.unix_timestamp());
        assert!(expiry <= before.unix_timestamp() + 30 * 60 + 5);
        // And it is exactly the documented HMAC of the username.
        let expected = generate(
            "huddle-test-secret",
            OffsetDateTime::from_unix_timestamp(expiry).unwrap(),
        );
        assert_eq!(credential, expected.1);
    }

    #[test]
    fn url_scheme_detection() {
        assert!(is_turn_url("turn:turn.example.com:3478?transport=udp"));
        assert!(is_turn_url("turns:turn.example.com:5349?transport=tcp"));
        assert!(!is_turn_url("stun:stun.example.com:3478"));
        assert!(!is_turn_url("turn.example.com:3478"));
    }
}
