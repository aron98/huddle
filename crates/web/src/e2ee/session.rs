//! The per-call E2EE state machine (ADR 010, D3–D5).
//!
//! `crate::call` drives this on raw `ServerMessage`s: membership and
//! epoch events from the outside, keying envelopes relayed by the
//! server. All key material lives client-side — the server only ever
//! relays opaque payloads between participants.

use crate::e2ee::chat;
use crate::e2ee::identity::{self, Identity};
use crate::e2ee::keying::{self, KeyingPayload};
use crate::e2ee::safety;
use crate::rtc::RtcClient;
use huddle_protocol::{Participant, ParticipantId, ServerMessage};
use std::collections::{HashMap, HashSet};
use web_sys::CryptoKey;

type Browser = RtcClient<crate::rtc::BrowserSignalingStream, crate::rtc::BrowserPeerConnection>;

/// What a peer looks like once the pairwise ECDH has produced a key:
/// the safety number is comparable out of band, and the peer may
/// already carry a verified mark (persisted from earlier meetings or
/// confirmed live).
#[derive(Clone)]
pub struct PeerView {
    pub id: ParticipantId,
    pub safety_number: Option<String>,
    pub verified: bool,
}

/// Everything we know about one remote participant.
struct Peer {
    public_bytes: Vec<u8>,
    fingerprint: String,
    /// The AES-128-GCM key derived from the pairwise ECDH; `None`
    /// until their identity announce arrives.
    pairwise_key: Option<CryptoKey>,
    /// The 12-digit safety number (unformatted).
    safety_digits: String,
    /// Their current sender key and the epoch it is valid for.
    sender_key: Option<(u64, [u8; keying::SENDER_KEY_LEN])>,
    verified: bool,
}

/// One meeting's E2EE state, owned by the session task.
pub struct E2eeSession {
    you: ParticipantId,
    identity: Identity,
    epoch: u64,
    /// Our sender key for the current epoch (lazily generated).
    own_key: Option<[u8; keying::SENDER_KEY_LEN]>,
    peers: HashMap<ParticipantId, Peer>,
    /// Fingerprints verified in earlier meetings + this one.
    verified_fingerprints: HashSet<String>,
    /// Bumped on every key change (own or peer, generation or drop) so
    /// the session task can cheaply tell when the transform worker
    /// needs a fresh key table (ADR 011, D1 sync).
    keys_version: u64,
}

/// A snapshot of the current-epoch media keys for the transform
/// worker: our send key (KID + base key) and every KID we can decrypt
/// with. The SFrame key/salt derivation happens in the glue, not here.
pub struct MediaKeys {
    pub version: u64,
    pub send: Option<(u32, [u8; keying::SENDER_KEY_LEN])>,
    pub recv: Vec<(u32, [u8; keying::SENDER_KEY_LEN])>,
}

impl E2eeSession {
    /// Load the long-term identity and the persisted verified set.
    /// `None` (with the call continuing) when the browser cannot do
    /// E2EE — no IndexedDB / WebCrypto, or a broken store.
    pub async fn start(you: ParticipantId, epoch: u64) -> Option<Self> {
        let identity = match identity::load_or_create().await {
            Ok(identity) => identity,
            Err(e) => {
                leptos::logging::warn!("e2ee identity unavailable: {e}");
                return None;
            }
        };
        let verified_fingerprints = identity::load_verified().await;
        Some(Self {
            you,
            identity,
            epoch,
            own_key: None,
            peers: HashMap::new(),
            verified_fingerprints,
            keys_version: 0,
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The roster-facing projection: safety numbers exist only once
    /// the pairwise key does (comparing before that would compare
    /// nothing).
    pub fn peer_views(&self) -> Vec<PeerView> {
        self.peers
            .iter()
            .map(|(id, peer)| PeerView {
                id: *id,
                safety_number: peer
                    .pairwise_key
                    .is_some()
                    .then(|| safety::format_safety_number(&peer.safety_digits)),
                verified: peer.verified,
            })
            .collect()
    }

    /// Whether our own sender key for the current epoch exists yet —
    /// the e2e probe asserts keying has actually happened.
    pub fn has_sender_key(&self) -> bool {
        self.own_key.is_some()
    }

    /// The key-table version: the session task re-posts the worker's
    /// key table whenever this changes (ADR 011, D1).
    pub fn keys_version(&self) -> u64 {
        self.keys_version
    }

    /// The current-epoch media keys for the transform worker (ADR 011,
    /// D2): our send KID and every KID we can decrypt with. Stale-epoch
    /// peer keys are filtered out defensively.
    pub fn media_keys(&self) -> MediaKeys {
        let you = safety::kid(self.epoch, self.you);
        let mut recv = Vec::with_capacity(self.peers.len() + 1);
        if let Some(key) = self.own_key {
            recv.push((you, key));
        }
        for (id, peer) in &self.peers {
            if let Some((epoch, key)) = peer.sender_key
                && epoch == self.epoch
            {
                recv.push((safety::kid(self.epoch, *id), key));
            }
        }
        MediaKeys {
            version: self.keys_version,
            send: self.own_key.map(|key| (you, key)),
            recv,
        }
    }

    /// Encrypt a chat body for the current epoch (ADR 011, D7). `None`
    /// means "do not send": without a key the message is refused with
    /// a status notice rather than falling back to plaintext.
    pub async fn encrypt_chat(&mut self, body: &str) -> Option<String> {
        let key = match self.ensure_own_key() {
            Ok(key) => key,
            Err(e) => {
                leptos::logging::warn!("cannot generate a chat key: {e}");
                return None;
            }
        };
        let crypto_key = match keying::import_aes_key(&chat::chat_key(&key)).await {
            Ok(key) => key,
            Err(e) => {
                leptos::logging::warn!("cannot import the chat key: {e}");
                return None;
            }
        };
        let nonce = keying::random_bytes(chat::NONCE_LEN).ok()?;
        let aad = chat::chat_aad(self.you, self.epoch);
        let ciphertext = keying::seal(&crypto_key, &nonce, &aad, body.as_bytes())
            .await
            .ok()?;
        Some(chat::encode(&nonce, &ciphertext))
    }

    /// Decrypt a chat body from `sender`, or render the placeholder for
    /// anything we cannot open — pre-join history, a stale epoch, a key
    /// race. Forward secrecy means the text stays sealed (ADR 011, D7).
    pub async fn decrypt_chat(&self, sender: ParticipantId, body: &str) -> String {
        const PLACEHOLDER: &str =
            "Encrypted message (no key available — sent before you joined or during a key change)";
        let Some((nonce, ciphertext)) = chat::decode(body) else {
            return PLACEHOLDER.to_owned();
        };
        let base = if sender == self.you {
            self.own_key
        } else {
            self.peers
                .get(&sender)
                .and_then(|peer| peer.sender_key)
                .filter(|(epoch, _)| *epoch == self.epoch)
                .map(|(_, key)| key)
        };
        let Some(base) = base else {
            return PLACEHOLDER.to_owned();
        };
        let Ok(crypto_key) = keying::import_aes_key(&chat::chat_key(&base)).await else {
            return PLACEHOLDER.to_owned();
        };
        let aad = chat::chat_aad(sender, self.epoch);
        match keying::open(&crypto_key, &nonce, &aad, &ciphertext).await {
            Ok(plaintext) => {
                String::from_utf8(plaintext).unwrap_or_else(|_| PLACEHOLDER.to_owned())
            }
            Err(_) => PLACEHOLDER.to_owned(),
        }
    }

    /// Announce our identity to every current roster participant.
    pub async fn announce_to_all(&mut self, roster: &[Participant], rtc: &mut Browser) {
        for participant in roster {
            if participant.id != self.you {
                self.announce(participant.id, rtc).await;
            }
        }
    }

    async fn announce(&self, to: ParticipantId, rtc: &mut Browser) {
        let payload = keying::encode_identity(&self.identity.public_bytes);
        if let Err(e) = rtc.send_e2ee_keying(to, self.epoch, payload).await {
            leptos::logging::warn!("e2ee announce to {to} failed: {e}");
        }
    }

    /// Feed one server message; only E2EE-related variants act.
    pub async fn handle_server_message(&mut self, msg: &ServerMessage, rtc: &mut Browser) {
        match msg {
            ServerMessage::ParticipantJoined { participant } if participant.id != self.you => {
                self.announce(participant.id, rtc).await;
            }
            ServerMessage::ParticipantLeft { id } => {
                self.peers.remove(id);
            }
            ServerMessage::E2eeEpoch { epoch } if *epoch != self.epoch => {
                self.rotate_epoch(*epoch, rtc).await;
            }
            ServerMessage::E2eeKeying {
                from,
                to,
                epoch,
                payload,
            } if to == &self.you => {
                self.handle_envelope(*from, *epoch, payload, rtc).await;
            }
            _ => {}
        }
    }

    /// A new epoch: drop every key scoped to the old one and
    /// redistribute fresh material to the remaining participants
    /// (ADR 010, D3 — meeting-scoped forward secrecy).
    async fn rotate_epoch(&mut self, epoch: u64, rtc: &mut Browser) {
        self.epoch = epoch;
        self.own_key = None;
        for peer in self.peers.values_mut() {
            peer.sender_key = None;
        }
        self.keys_version += 1;
        let targets: Vec<ParticipantId> = self.peers.keys().copied().collect();
        for to in targets {
            self.send_own_key(to, rtc).await;
        }
    }

    async fn handle_envelope(
        &mut self,
        from: ParticipantId,
        wire_epoch: u64,
        payload: &str,
        rtc: &mut Browser,
    ) {
        match keying::decode(payload) {
            Some(KeyingPayload::Identity { public }) => {
                self.learn_identity(from, public, rtc).await;
            }
            Some(KeyingPayload::SenderKey { nonce, ciphertext }) => {
                self.receive_sender_key(from, wire_epoch, &nonce, &ciphertext)
                    .await;
            }
            Some(KeyingPayload::Verified { nonce, ciphertext }) => {
                self.receive_verified_confirmation(from, &nonce, &ciphertext)
                    .await;
            }
            None => leptos::logging::warn!("dropped a malformed keying envelope from {from}"),
        }
    }

    /// Learn (or re-learn) a peer's identity: derive the pairwise key,
    /// compute the safety number, and immediately hand them our
    /// current sender key.
    async fn learn_identity(&mut self, from: ParticipantId, public: Vec<u8>, rtc: &mut Browser) {
        if self
            .peers
            .get(&from)
            .is_some_and(|peer| peer.public_bytes == public)
        {
            return;
        }
        if public.len() != safety::PUBLIC_KEY_LEN {
            leptos::logging::warn!("peer {from} announced a malformed public key");
            return;
        }
        let fingerprint = safety::fingerprint(&public);
        let verified = self.verified_fingerprints.contains(&fingerprint);
        let info = safety::pairwise_info(&self.identity.fingerprint, &fingerprint);
        let pairwise_key =
            match keying::derive_pairwise_key(&self.identity.private_key, &public, &info).await {
                Ok(key) => key,
                Err(e) => {
                    leptos::logging::warn!("pairwise derivation with {from} failed: {e}");
                    return;
                }
            };
        let safety_digits = safety::safety_number(&self.identity.public_bytes, &public);
        self.peers.insert(
            from,
            Peer {
                public_bytes: public,
                fingerprint,
                pairwise_key: Some(pairwise_key),
                safety_digits,
                sender_key: None,
                verified,
            },
        );
        self.send_own_key(from, rtc).await;
    }

    /// Open and accept a peer's sender key. The epoch inside the
    /// ciphertext must match both the envelope's epoch label and our
    /// current epoch — a relayed stale envelope is refused.
    async fn receive_sender_key(
        &mut self,
        from: ParticipantId,
        wire_epoch: u64,
        nonce: &[u8],
        ciphertext: &[u8],
    ) {
        let Some(peer) = self.peers.get(&from) else {
            return;
        };
        let Some(pairwise_key) = peer.pairwise_key.clone() else {
            return;
        };
        let aad = keying::sender_key_aad(from, self.you);
        let Ok(plaintext) = keying::open(&pairwise_key, nonce, &aad, ciphertext).await else {
            leptos::logging::warn!("could not open the sender key from {from}");
            return;
        };
        let Some((inner_epoch, key)) = keying::decode_sender_key_plaintext(&plaintext) else {
            return;
        };
        if inner_epoch != wire_epoch || inner_epoch != self.epoch {
            leptos::logging::warn!("refused a sender key for a stale epoch from {from}");
            return;
        }
        if let Some(peer) = self.peers.get_mut(&from) {
            peer.sender_key = Some((inner_epoch, key));
            self.keys_version += 1;
        }
    }

    /// A peer confirmed the safety-number comparison: accept the mark
    /// only if the confirmation decrypts (with our pairwise key) to
    /// *our* safety number.
    async fn receive_verified_confirmation(
        &mut self,
        from: ParticipantId,
        nonce: &[u8],
        ciphertext: &[u8],
    ) {
        let Some(peer) = self.peers.get(&from) else {
            return;
        };
        if peer.pairwise_key.is_none() {
            return;
        }
        let fingerprint = peer.fingerprint.clone();
        let safety_digits = peer.safety_digits.clone();
        let Some(pairwise_key) = peer.pairwise_key.clone() else {
            return;
        };
        let aad = keying::verified_aad(from, self.you);
        let Ok(plaintext) = keying::open(&pairwise_key, nonce, &aad, ciphertext).await else {
            leptos::logging::warn!("could not open the verification from {from}");
            return;
        };
        let digits = String::from_utf8(plaintext).unwrap_or_default();
        if digits != safety_digits {
            return;
        }
        self.verified_fingerprints.insert(fingerprint);
        if let Err(e) = identity::store_verified(&self.verified_fingerprints).await {
            leptos::logging::warn!("could not persist the verified mark: {e}");
        }
        if let Some(peer) = self.peers.get_mut(&from) {
            peer.verified = true;
        }
    }

    /// The user compared the numbers out of band and they match:
    /// persist the mark locally and tell the peer (encrypted, so the
    /// server learns nothing about who verified whom).
    pub async fn confirm_verification(&mut self, peer_id: ParticipantId, rtc: &mut Browser) {
        let Some(peer) = self.peers.get(&peer_id) else {
            return;
        };
        let Some(pairwise_key) = peer.pairwise_key.clone() else {
            leptos::logging::warn!("cannot verify before the key exchange completes");
            return;
        };
        let safety_digits = peer.safety_digits.clone();
        let Ok(nonce) = keying::random_bytes(keying::NONCE_LEN) else {
            return;
        };
        let aad = keying::verified_aad(self.you, peer_id);
        let Ok(ciphertext) =
            keying::seal(&pairwise_key, &nonce, &aad, safety_digits.as_bytes()).await
        else {
            return;
        };
        let payload = keying::encode_verified(&nonce, &ciphertext);
        if let Err(e) = rtc.send_e2ee_keying(peer_id, self.epoch, payload).await {
            leptos::logging::warn!("could not send the verification to {peer_id}: {e}");
            return;
        }
        let fingerprint = self
            .peers
            .get(&peer_id)
            .map(|peer| peer.fingerprint.clone());
        if let Some(fingerprint) = fingerprint {
            self.verified_fingerprints.insert(fingerprint);
            if let Err(e) = identity::store_verified(&self.verified_fingerprints).await {
                leptos::logging::warn!("could not persist the verified mark: {e}");
            }
        }
        if let Some(peer) = self.peers.get_mut(&peer_id) {
            peer.verified = true;
        }
    }

    /// Send our current-epoch sender key to one peer, generating it
    /// first if needed (ADR 010, D2: fresh per epoch, sealed to the
    /// receiver's pairwise key).
    async fn send_own_key(&mut self, to: ParticipantId, rtc: &mut Browser) {
        let Some(peer) = self.peers.get(&to) else {
            return;
        };
        let Some(pairwise_key) = peer.pairwise_key.clone() else {
            return;
        };
        let key = match self.ensure_own_key() {
            Ok(key) => key,
            Err(e) => {
                leptos::logging::warn!("could not generate a sender key: {e}");
                return;
            }
        };
        let Ok(nonce) = keying::random_bytes(keying::NONCE_LEN) else {
            return;
        };
        let plaintext = keying::sender_key_plaintext(self.epoch, &key);
        let aad = keying::sender_key_aad(self.you, to);
        let Ok(ciphertext) = keying::seal(&pairwise_key, &nonce, &aad, &plaintext).await else {
            return;
        };
        let payload = keying::encode_sender_key(&nonce, &ciphertext);
        if let Err(e) = rtc.send_e2ee_keying(to, self.epoch, payload).await {
            leptos::logging::warn!("could not send our sender key to {to}: {e}");
        }
    }

    fn ensure_own_key(&mut self) -> Result<[u8; keying::SENDER_KEY_LEN], String> {
        if let Some(key) = self.own_key {
            return Ok(key);
        }
        let bytes = keying::random_bytes(keying::SENDER_KEY_LEN)?;
        let key: [u8; keying::SENDER_KEY_LEN] = bytes
            .try_into()
            .map_err(|_| "wrong random length".to_owned())?;
        self.own_key = Some(key);
        self.keys_version += 1;
        Ok(key)
    }
}
