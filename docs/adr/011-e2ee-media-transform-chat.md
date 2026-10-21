# ADR 011: E2EE media transform and encrypted chat

- Status: Accepted (issue #28)
- Date: 2026-10-21
- Relates to: ADR 009 (E2EE spike), ADR 010 (key exchange)
- Supersedes: none

## Context

ADR 010 shipped the meeting-scoped key exchange: pairwise ECDH identities,
sender keys per epoch, safety numbers. The handoff to this issue was the
spike verdict of ADR 009, item 2: SFrame (RFC 9605) framing over
`RTCRtpScriptTransform` workers, with in-band KID+CTR (receiver frame counters
are not safe IVs — probe-proven in the spike), a header-clear convention so
the SFU's VP8 keyframe classification keeps reading the true first payload
byte, and a WebCrypto-only crypto stack (no WASM crypto). Issue #27 also
added the chat relay; this issue makes chat bodies opaque to the server in
E2EE meetings and decides what the database keeps.

## Decision

### D1: Worker architecture

One classic `Worker` per call, a vendored plain-JS asset at
`/assets/e2ee/e2ee-worker.js` (same pattern as the blur shim, ADR 007; trunk's
`copy-dir` ships it, rust-embed bakes it into release builds). It is loaded
by same-origin **URL**, never a `Blob:` URL — the served CSP has no
`worker-src`, so worker loads fall back to `script-src 'self'`, which admits
the URL form and would reject the Blob form.

The web-sys bindings for `RTCRtpScriptTransform` exist but sit behind
`#[cfg(web_sys_unstable_apis)]`, which the fixed wasm-gate command cannot
enable. The main-thread Rust glue therefore feature-detects and constructs the
transform handles through raw JS interop (`Reflect::get/construct/set`); the
`rtctransform` event itself fires in the worker, where the stream pipeline
lives. ADR 009's phrase "constructed in the worker" refers to that pipeline —
the spec mandates main-thread construction of the `RTCRtpScriptTransform`
handle itself.

Each sender and receiver gets one `RTCRtpScriptTransform` bound to the shared
worker (instances are not reusable). Sender transforms attach at join, before
`publish()`; receiver transforms attach in the `ontrack` handler (screen-share
senders attach when created; `replaceTrack` keeps senders, so transforms
survive track swaps).

### D2: SFrame parameters

- Cipher suite `AES_128_GCM_SHA256_128` (RFC 9605, suite value `0x0004`).
- Base key: the sender key of each participant for the current epoch
  (ADR 010, D2).
- `sframe_key`/`sframe_salt` derived per RFC 9605 §4.4.2 with HKDF-SHA256 —
  computed on the main thread in **pure Rust** (`e2ee::safety::hkdf_sha256`)
  and pinned against the RFC's published test vector. The worker only
  `importKey`s the derived bytes and runs `encrypt`/`decrypt`.
- Header form: fixed-length, config byte `0xBE` ‖ KID (4 bytes, big-endian)
  ‖ CTR (7 bytes, big-endian) — 12 bytes total, as budgeted in ADR 009.
  KID is the ADR 010 formula `SHA-256("huddle-e2ee/v1/kid" ‖ epoch ‖ sender)`,
  already implemented in `safety::kid`.
- Nonce = `sframe_salt XOR encode(CTR, 12)`; the sender counts CTR from 0 per
  KID. The 7-byte field bounds a key at 2^56 frames.
- AAD = the 12-byte header.

### D3: Header-clear framing

Wire payload: `clear codec prefix ‖ SFrame header ‖ ciphertext‖tag`.
VP8 prefix: 10 bytes for a keyframe, 3 for a delta (parsed from the payload
descriptor's P bit — the same byte the spike proved the SFU reads for
keyframe classification). Opus prefix: the 1-byte TOC. Sender and receiver
parse identically, so the receiver can locate the header without a length
preamble. The clear prefix is **not authenticated** — the tradeoff already
accepted in ADR 009; the attacker model (the SFU) could flip the prefix at
the cost of corrupting which keyframe/delta structure the decoder sees, but
cannot read or forge payload content.

Keyframe requests (PLI/FIR): the browser generates keyframes on PLI
automatically and the SFU relays them unchanged, so senders honor them
unconditionally with **no code change**; the header-clear convention is what
keeps the SFU's own keyframe heuristics truthful in the first place.

### D4: Failure behavior — drop, never plaintext

- Senders **drop** frames until their send key exists (nothing leaves the
  browser unencrypted in an E2EE meeting; receivers see a brief black period
  while keying completes).
- Receivers **drop** undecryptable frames and count them in a
  `decryptFail` stat (genuine key races surface as a small transient, not as
  garbled media).

### D5: Degrade detection

The worker tracks a per-frame transform-time EMA (α ≈ 0.1). EMA > 20 ms for
30 consecutive frames posts a one-shot `degrade` message; the call shows
"E2EE is struggling on this device" with the suggestion to leave and start a
regular meeting. A true plaintext rejoin **cannot** be offered inside an
E2EE-flagged meeting: the server's trust mode is fixed at creation (ADR 010
interlock), so the honest remedy is leaving, not silently downgrading the
meeting's security promise.

### D6: Browser floor and gating

RTCRtpScriptTransform shipped in Chrome/Edge 141, Firefox 117, Safari 15.4.
An incapable browser in an E2EE meeting is **refused at join** with a clear
message (the alternative — sending media nobody could decrypt — is a broken
call). Feature detection: `RTCRtpScriptTransform` constructor, `transform` on
sender/receiver prototypes, and `Worker`.

### D7: E2EE chat

Encrypted chat rides the **existing broadcast chat path** — `ClientMessage::
Chat{body}` with `body = base64(0x04 ‖ nonce12 ‖ AES-GCM ct)` — not the
directed keying relay. Chat is broadcast (the relay is per-recipient), and
the chat path is the only one that persists (so rejoiners get history) and
relays to everyone in one shot. The server constructs, persists, and relays
the body verbatim — it never inspects bodies (audited; pinned by a server
test). The envelope tag `0x04` continues keying's tag-byte convention.

- Chat key = `HKDF-SHA256(sender_key, info="huddle-e2ee/v1/chat", 16)` — HKDF
  separation from the media context.
- AAD = `"huddle-e2ee/v1/chat-message" ‖ sender_uuid ‖ epoch_le64` — binds
  the claimed sender and epoch; receivers only try current-epoch keys.
- A sender without a key refuses to send (status notice), never falls back to
  plaintext.
- Messages that cannot be decrypted (sent before you joined or in an earlier
  epoch, or a transient key race) render as a placeholder — forward secrecy
  means newcomers cannot read past epochs, consistent with ADR 010 D3.
- System messages (`sender_id: None`) stay plaintext: they are
  server-generated. The only emitter is the recording-consent notice, which is
  interlocked off in E2EE meetings anyway — intentionally server-visible.

### D8: Persistence

E2EE meetings: chat bodies are encrypted client-side and `chat_messages`
stores **ciphertext only**. Plaintext meetings keep today's behavior exactly:
trust mode is an organizer-chosen per-meeting product flag, and forcing
encryption everywhere would mean ungating the keying relay for all meetings
and breaking documented plaintext behavior (organizers may want readable
history). The honest floor is documented: in E2EE meetings the operator still
sees message timing, sizes, and sender names — content only is protected.

No migration: `body` is already an opaque `TEXT` column.

### D9: Deferred: rekey cadence

ADR 009 proposed rekeying after 2^24 frames or 24 h under one key. Deferred:
the 7-byte CTR bounds nonce reuse at 2^56 frames, and every join/leave
rotates the epoch (and with it all sender keys, ADR 010 D3). A mid-epoch
sender-key ratchet would need a KID generation component (the KID formula is
fixed per epoch+sender, so a ratcheted key would collide with the old one);
that is recorded as a future item rather than smuggled into this issue.

### D10: Testing

- Native: RFC 9605 test-vector pin of the key/salt/nonce derivation; header
  and chat-envelope framing tests.
- E2E: media still flows with transforms attached (ssrc/framesDecoded grow),
  `__huddleE2eeStats.transform` asserts real crypto counters
  (`decryptFail === 0`), chat round-trips in both directions, a third joiner
  sees the placeholder for pre-join history (proving ciphertext persistence),
  the `?e2eeDegrade=1` knob trips the degrade path, and `?e2eeTag=1` proves
  the whole transform/SFU plumbing in a plaintext meeting with a no-crypto
  tag transform.
- No SFU sans-io byte-forwarding test: covered by the e2e media flow plus the
  ADR 009 source analysis (the SFU never touches payload bytes beyond VP8
  classification, which the header-clear convention preserves).

## Consequences

- Media in E2EE meetings is opaque to the SFU (the main point); the SFU's
  codec-aware features keep working because of D3.
- The worker asset is a second vendored JS file (maintenance surface), kept
  deliberately small and framework-free.
- Old clients are excluded from E2EE meetings explicitly, not silently.
- The operator floor for E2EE chat becomes metadata-only (D8).
