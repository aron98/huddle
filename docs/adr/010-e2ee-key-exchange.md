# ADR 010: Participant key exchange and safety-number verification

- Status: proposed (implements issue #27; stacked on ADR 009's spike branch)
- Date: 2026-10-21

## Context

ADR 009 fixed the *frame format* end-to-end encryption must use (SFrame
RFC 9605, `AES_GCM_128_FULL_TAG`, in-band KID/CTR, header-clear
convention) and proved the SFU can forward ciphertext untouched. What it
deliberately left open — and what issue #27 requires — is everything
upstream of the frames:

1. how two participants agree on key material such that **the server
   only ever relays ciphertext it cannot use** (pairwise key agreement
   with long-term identity keys),
2. how membership changes rotate that material (epochs, forward
   secrecy),
3. how participants gain confidence the server did not silently insert
   itself into (1) — the out-of-band **safety-number verification UX**,
4. and the honest **threat model** of what remains possible for a
   malicious server either way.

Scope note: this branch lands the trust-mode flag, the key exchange,
the relay, the safety numbers, and the verification UX. The transform
worker that *consumes* the keys (encrypting frames, E2EE chat) is #28.

## Decision 1 — Identity keys: WebCrypto ECDH P-256, non-extractable, IndexedDB

Each participant owns a **long-term identity keypair** generated once,
client-side, and persisted across meetings. We use **WebCrypto ECDH on
P-256** with the private key generated `extractable: false` and stored —
via structured clone — in **IndexedDB** (`huddle-identity` DB, `keys`
store). The public key is exported as the raw uncompressed point
(65 bytes) and travels over the keying relay (Decision 4).

The issue text says X25519; we deviate deliberately. The options:

- **WebCrypto X25519** — the *right* primitive (matches the issue, and
  the SFrame/MLS ecosystem), but the browser floor is recent and uneven
  (Chrome 133+, Safari 17+, Firefox 129+ as of 2026). Requiring it would
  exclude otherwise-capable browsers that pass ADR 009's transform gate.
- **x25519-dalek compiled to wasm** — universal in principle, but the
  private key then lives as ordinary bytes in WASM/JS memory: any XSS
  payload can exfiltrate it, and we must build our own at-rest story.
  Key hygiene is strictly worse than the non-extractable WebCrypto path.
- **WebCrypto ECDH P-256, non-extractable** — supported by every
  browser that also passes the ADR 009 feature detect, and the
  `extractable: false` flag means the private key **cannot be read out
  of the browser at all**, by us or by an attacker who achieves script
  execution. IndexedDB structured clone persists `CryptoKey` objects
  natively. `deriveBits` still works against a non-extractable key —
  that is exactly what it is for.

What the safety number and the sender-key envelope need from the
agreement primitive is the computational-Diffie-Hellman property, which
P-256 provides. Deviation from the issue's X25519 is therefore allowed
by the issue's own wording ("the ECDH property is what matters") and is
justified by the browser floor plus the key-hygiene argument above. If
WebCrypto X25519 becomes universally available we can add it as an
additional curve later; the wire format (raw public key bytes +
explicit curve in the payload tag) leaves room.

**Fingerprint**: SHA-256 of the raw public key. Used as the client-side
keying for verified state, never shown to users directly (the safety
number is the human artifact).

## Decision 2 — Key agreement: SFrame §9-style sender keys, distributed pairwise

ADR 009 requires per-epoch **SFrame base keys**. The candidates for
distributing them:

- **MLS tree** — the asymptotically best answer (O(log N) rekey, strong
  FS/PCS guarantees, standardized credentials). But it requires a full
  MLS stack in the browser (wasm or JS), group state machinery, and a
  delivery service contract we do not need. huddle meetings are small
  (capacity-bounded roster, single-digit to low-double-digit N); O(N)
  messages per membership change are a few kilobytes.MLS is a
  dependency and complexity budget we decline to spend in v1.
- **Direct pairwise media keys only** (each pair derives its media key
  from the pairwise ECDH) — simplest, but then every receiver needs a
  *sender-specific* key per remote sender anyway, which is exactly the
  SFrame sender-key shape, just without the standard framing story.
- **SFrame §9 sender keys** (chosen) — each *sender* generates a fresh
  random 128-bit **sender key** per epoch and distributes it, encrypted
  to each receiver's identity key, through the relay. Receivers build
  the SFrame base-key table (KID → key) that ADR 009's transform
  worker consumes. This matches RFC 9605 §9's design intent, needs no
  new crypto dependencies, and keeps the per-frame story entirely
  standard SFrame.

**KID derivation** (consumed by #28): `KID = SHA-256("huddle-e2ee/v1/kid"
‖ epoch_le64 ‖ sender_id) mod 2³²`. The receiver learns the
KID → (sender, epoch, key) mapping from the keying bookkeeping itself,
so it never re-derives anything; collisions are a 2⁻³² birthday event
over a dozen-ish live keys and are detectable (two senders claiming one
KID) — out of scope for v1, documented here.

**Pairwise derivation**: the sender-key envelope and the
verified-confirmation are encrypted with AES-128-GCM under
`K_pairwise = HKDF-SHA256(ECDH(our priv, their pub), salt = "", info =
"huddle-e2ee/v1/pairwise" ‖ fp_low ‖ fp_high)` where `fp_low/fp_high`
are the two identity fingerprints in sorted order — canonicalized so
both sides derive the same key. HKDF is implemented in pure Rust
(`sha2` + `hmac`, ~30 lines, the same RFC 5869 extract-and-expand
construction, natively unit-testable against the RFC 5869 test
vectors): the WebCrypto HKDF *algorithm* has patchy browser support
(notably absent in older Safaris), so it is still avoided. The two
crates compile to wasm and ship in the bundle; no WASM crypto crate
is needed anywhere.

## Decision 3 — Epochs: server-scoped counter, server as labeler (never key holder)

Every meeting instance carries a `u64` **epoch counter** maintained by
the SFU engine. It starts at 0 and increments on **every join and every
leave**. The joiner reads the current epoch in `Welcome`; everyone else
learns of the bump via a broadcast `e2ee_epoch` message. The value is
identical on all sides by construction — clients do not count events
themselves (a missed roster event would desynchronize key distribution;
the server's counter is the one authority).

On every epoch change each participant generates a fresh sender key
for the new epoch and distributes it to all current peers (Decision 2).
This yields the meeting-scoped forward secrecy the issue asks for:

- a **leaver** never receives new-epoch sender keys → cannot decrypt
  any subsequent epoch, even with recorded keying traffic;
- a **newcomer** only receives keys for epochs ≥ its join → cannot
  decrypt past epochs, even from a full relay transcript.

The server's role is deliberately minimal: it *labels* epochs and
*relays* envelopes. It never generates, encrypts, decrypts, or stores
key material. The epoch is a routing/consistency artifact, not a secret.

Cadence beyond membership changes (ADR 009's 2²⁴-frame / 24 h
rotation) is #28's to enforce, riding the same epoch machinery.

## Decision 4 — Relay shape: dedicated WS message types, opaque payloads

ADR 009's open question: new WS message type vs. chat-path envelope.
**Chosen: dedicated messages.** Rationale: chat bodies are persisted
(`chat_messages` table, history replay) — keying traffic must never be
stored or replayed, so riding the chat path invites accidental
persistence and history-replay bugs. A dedicated type keeps the server
handler explicit (no persistence, directed routing, e2ee-gated) and is
additive-safe either way, so we take the safer shape.

Wire (all additive, `#[serde(default)]` where noted):

- `ClientMessage::E2eeKeying { to, epoch, payload }` — `to` is a
  participant id, `payload` a base64 `String` the server treats as
  **opaque bytes** (ADR 009's codec-agnostic requirement — the server
  must not become a crypto dependency).
- `ServerMessage::E2eeKeying { from, to, epoch, payload }` — relayed to
  `to` only.
- `ServerMessage::E2eeEpoch { epoch }` — broadcast on membership change
  (E2EE meetings only).
- `Welcome` gains `e2ee_enabled: bool` and `e2ee_epoch: u64` (flat,
  `#[serde(default)]`, mirroring the additive `recording` flag — a
  nested `MeetingE2ee` struct was ADR 009's sketch; flat fields are the
  established pattern here).

Server-side rules (defense in depth, each independently testable): the
sender must be a live participant of the meeting; `to` must be a live
participant (drop silently otherwise — an offline peer is not an
error); the meeting must be E2EE; the payload is capped (16 KiB) and
never persisted; routing is directed, never broadcast, by the server.
Rate limiting matches the chat path (connection-level size/message
caps) — chat has no per-message token bucket either; the relay's
directed routing bounds amplification to 1×.

**Client-side payload format** (defined here, invisible to the server):
base64 of `[tag] ‖ data`, tags:

- `0x01` identity announce — data = raw 65-byte P-256 public key.
  Sent on join to every roster peer, and to each newcomer on
  `ParticipantJoined`.
- `0x02` sender key — data = 12-byte nonce ‖ AES-128-GCM ciphertext of
  `epoch_le64 ‖ sender_key(16 bytes)` with AAD
  `"huddle-e2ee/v1/sender-key" ‖ sender_id ‖ receiver_id`. The epoch is
  bound *inside* the ciphertext so a malicious server cannot replay a
  captured envelope under a different epoch label.
- `0x03` verified confirmation — data = 12-byte nonce ‖ AES-128-GCM
  ciphertext of the 12-digit safety-number string with AAD
  `"huddle-e2ee/v1/verified" ‖ sender_id ‖ receiver_id`. Produced with
  the pairwise key, it proves the sender derived the same pairwise
  secret; the receiver marks the peer verified.

The tag lives in the payload, not the message type, so the relay stays
a generic pipe — #28's E2EE chat can add tags without a protocol
change.

## Decision 5 — Safety numbers: 12 digits, identity-bound, verified marks exchanged

Derivation: sort the two raw identity public keys lexicographically
(canonical across sides), then
`SHA-256("huddle-e2ee/v1/safety-number" ‖ key_a ‖ key_b)`, take the
first 40 bits big-endian, `n mod 10¹²`, format as 12 zero-padded
digits displayed in four triplets: `123 456 789 012`.

Justification of the format:

- **Why digits, not a word list**: numeric SAS is the Signal/WhatsApp
  convention users may recognize; it reads aloud unambiguously over a
  phone call in any language, with no localization surface. A PGP-style
  word list needs per-language curation for no extra security at this
  length.
- **Why 12 digits / ≈40 bits**: a MITM attacker must find an ECDH
  keypair whose *pairwise* safety number collides — each attempt is a
  fresh key generation plus hash, ≈2⁴⁰ expected work for a 50 % chance.
  That is far beyond any real-time attack window for an interactive
  call, while 6 digits (≈20 bits) would fall to a patient adversary.
  (The `mod 10¹²` mapping has a ~9 % top-of-range bias; irrelevant —
  the number is compared for *equality between the two honest
  endpoints*, never sampled uniformly against a guess.)
- **Why identity-bound, not epoch-bound**: the number pins the
  *long-term identity pair*, so verification state persists across
  meetings in the same browser profile — verify a colleague once, the
  roster badge stays. Epoch key material is covered separately: an
  attacker who somehow pinned the identity channel cannot leverage
  that into epoch keys, since sender keys are re-randomized per epoch.

**Verification UX** (v1: the string alone; QR is optional and
deferred):

- Each roster entry in an E2EE meeting carries a **verify affordance**
  and, once verified, a persistent badge.
- Opening it shows the 12-digit safety number for that pair and a
  "Mark as verified" control. The instruction is to compare it out of
  band (read it to each other) — the dialog says so.
- Clicking "verified" does two things: (a) records the peer's
  fingerprint as verified in local IndexedDB (client-side state only),
  and (b) sends a `0x03` confirmation envelope to the peer, who marks
  *its* side. Both ends converge on the verified state without the
  server learning anything: the confirmation is an opaque relayed
  payload, indistinguishable from keying traffic.
- Verified state is keyed by **identity fingerprint**, so a peer
  returning with a different device/key shows as unverified — the
  badge degrades honestly.

## Decision 6 — Recording interlock (server-enforced)

Per ADR 009's product decision, E2EE and recording are **visibly
exclusive**:

- `POST /api/meetings/{code}/recording` on an E2EE meeting returns
  `409` with a new additive error code `e2ee_active` and human copy
  ("this meeting is end-to-end encrypted, so it cannot be recorded").
- The engine's `start_recording` refuses independently (defense in
  depth — even a future caller that skips the REST check cannot start
  the recorder on an E2EE instance).
- E2EE is set **at creation only** in v1 (`PATCH` does not touch it),
  which makes "a recording meeting cannot switch E2EE on" hold by
  construction. Relaxing to a runtime toggle is a later, separate
  decision (it needs an epoch bump + UI copy, and the honest UX copy
  per ADR 009 stays "start a regular meeting to record").

The call header shows a **lock indicator** for E2EE meetings next to
the REC indicator position; since the server refuses the pairing, the
two indicators never co-occur.

## Threat model

Written requirement of the issue. Adversary: **the server operator**
(huddle is SFU-topologied; the server is the natural malicious
intermediary). Other clients are outside this model — a malicious
*participant* sees and hears everything anyway.

**Without verification, a malicious server can:**

- **Key substitution / MITM.** The server controls the relay and the
  roster. It can announce its own identity keys to both sides of each
  pair, station itself as a hidden "participant" in every pairwise
  channel, decrypt each sender key, and re-encrypt toward the real
  receiver. The two honest endpoints still derive *a* shared channel —
  just with the server in the middle — and nothing on the wire looks
  wrong. **This is why "E2EE" without the verification step is only a
  promise**: the lock badge asserts the *mechanism* is active, not that
  the *peer* is who they claim.
- Frame-type/size metadata leaks (ADR 009's header-clear convention).
- Selective DoS: drop or corrupt keying envelopes or frames; E2EE
  fails *loudly* (decrypt errors), not silently — but a meeting that
  never establishes keys is indistinguishable from a broken client.
- **Flag lying**: the trust-mode flag is server-asserted. A server can
  create a meeting the organizer believes is E2EE while telling
  joining clients otherwise (or vice versa); clients cannot verify the
  organizer's intent from inside. Mitigation is out-of-band (the
  organizer sees the lock on their own screen); we surface the flag
  everywhere the UI can.

**With verification, the server can still:**

- **Traffic analysis**: who talks to whom and when (speaking events are
  still broadcast in the clear), frame timing, sizes and counts, RTP
  SSRC correlation across hops.
- **Metadata**: titles, schedules, roster, join/leave times, chat
  (plaintext on signaling until #28), passcode-protected-ness, the
  graph of who verifies whom *is* hidden (opaque payloads), but the
  *timing* of keying traffic is visible — a server can infer that
  verification happened around the moment a `0x03`-shaped envelope
  flows, just not between whom or with what outcome.
- **Denial of service** at any layer; **rollback** (serve old payloads
  — defeated by the additive-schema discipline and
  `PROTOCOL_VERSION`).
- Never: decrypt current-epoch media of a pair that verified, forge
  the verified badge (the confirmation requires the pairwise key), or
  decrypt past/future epochs for a participant who was absent (epoch
  forward secrecy).

**What the verification step adds**: exactly the gap above — absent
it, E2EE is honest-but-curious-server security; with it, key
substitution requires finding a collision against a 40-bit SAS inside
the interactive window, which is not a practical attack.

## Wire summary (all additive; `PROTOCOL_VERSION` unchanged)

- `MeetingDto.e2ee: bool` (`#[serde(default)]`) — REST list/detail,
  dashboard, prejoin.
- `CreateMeetingRequest.e2ee: bool` (`#[serde(default)]`) — organizer
  picks "End-to-end encrypted" at creation; not in
  `UpdateMeetingRequest`.
- `Welcome.e2ee_enabled: bool`, `Welcome.e2ee_epoch: u64`
  (`#[serde(default)]`).
- `ServerMessage::E2eeEpoch { epoch }`, `ClientMessage::E2eeKeying
  { to, epoch, payload }`, `ServerMessage::E2eeKeying { from, to,
  epoch, payload }`.
- `ErrorCode::E2eeActive` ("e2ee_active" on the wire) — recording
  refused on E2EE meetings.
- DB migration `0007_meeting_e2ee.sql`:
  `ALTER TABLE meetings ADD COLUMN e2ee BOOLEAN NOT NULL DEFAULT 0;`

## Test strategy

- **Protocol**: legacy-payload-still-parses tests for every new
  field/variant; round-trips asserting snake_case tags.
- **Server**: engine/session tests — keying relays only to `to`
  (invisible to third parties), epoch bumps and broadcast on
  join/leave, relay refused in plaintext meetings, payload cap, and
  the interlock (REST `409 e2ee_active` + engine-level refusal).
- **Web**: the relay plumbing (RtcClient event surface) is
  platform-neutral and covered by the native mock-server pattern;
  WebCrypto itself is covered by the e2e suite (the browser is the
  only honest oracle for WebCrypto behavior).
- **E2E** (`test/e2e/e2e-e2ee.mjs`, port 3137): unlike decryption, the
  safety number **is assertable** — two pages join an E2EE meeting and
  must derive the identical 12-digit string for each other; the
  verified-state flow (verify on one side → badge on both) is
  observable via `data-testid` probes and the `?test=1`
  `window.__huddleE2eeStats` hook.

## Consequences

**Positive**

- The server relays only ciphertext and opaque envelopes; key material
  it sees is unusable.
- Verification is assertable end-to-end (a rarity in E2EE testing) and
  converges on both sides without server knowledge.
- Membership-driven epochs give meeting-scoped forward secrecy with no
  client-side consensus problem (server-scoped counter).
- Everything is additive; plaintext meetings are byte-for-byte
  unaffected.

**Negative / accepted costs**

- O(N) sender-key envelopes per membership change — fine at huddle's
  meeting sizes, would not scale to hundreds (MLS is the documented
  future answer).
- P-256 instead of X25519 (browser floor; revisit when WebCrypto
  X25519 is universal).
- The trust-mode flag remains server-asserted (flag-lying attack
  documented, out of scope).
- Verification is a manual social step; the UX copy must keep saying
  so — the badge alone proves nothing about the peer.

**For #28 (riding this branch's keys)**: the sender-key table keyed by
(epoch, sender) with KID derivation is fixed here; the transform
worker consumes it via `postMessage`. E2EE chat adds a payload tag
(Decision 4) — no protocol change needed. The
`?e2ee=1` test knob, `window.__huddleE2eeStats`, and the degrade-path
metrics belong to #28's e2e scope; this branch exposes the epoch/key
bookkeeping needed to build them.
