# ADR 009: End-to-end media encryption via WebRTC Encoded Transform

- Status: proposed (spike outcome for issues #27, #28)
- Date: 2026-10-20

## Context

Issue #26 asks whether huddle can offer end-to-end encrypted (E2EE) media
while keeping the SFU topology. Today the media path is only hop-by-hop
secure: every peer terminates DTLS-SRTP against the server (str0m), so the
SFU reads and re-originates every media frame in the clear — for
depayloadizing, for simulcast layer selection, for the RTP recorder, and
for re-payloadizing toward each subscriber. The server is therefore in a
position to observe all media.

The standard escape hatch is the **WebRTC Encoded Transform** family
(`RTCRtpScriptTransform` on the worker-based path; the legacy
`createEncodedStreams` insertable-streams API in Chromium): the browser
hands the application *encoded, packetization-independent* frames to a
`Worker`, the application encrypts them, and the browser packetizes the
ciphertext. If the clients do that, the SFU forwards opaque bytes and
only the clients hold keys. Two layers result:

```
[inner]  sender client --AES-GCM over encoded frame--> receiver client
[outer]  sender client --DTLS-SRTP--> SFU --DTLS-SRTP--> receiver client
```

The spike questions: browser support floor (audio vs video), whether
str0m 0.24.1 can forward opaque payloads without breaking (simulcast
selection, BWE, PLI/FIR, keyframes), what in the huddle SFU reads payload
bytes, the crypto pipeline design, and the CPU cost of per-frame crypto
on low-end hardware. Conclusions feed #27 (key exchange) and #28 (E2EE
chat), which will be implemented on top of this branch.

## Verdict: GO (with one product decision and one accepted workaround)

E2EE media via `RTCRtpScriptTransform` is viable for huddle.

Rationale in one paragraph: every browser we need to support ships the
modern worker-based transform API for **both audio and video** (Chrome/Edge
141+, Firefox 117+, Safari 15.4+); we verified locally on
chrome-headless-shell 155 that a full audio+video loopback call encrypts
and decrypts every frame with zero failures using a plain transform
worker. str0m's media pipeline turns out to be almost entirely
content-agnostic for our negotiated codecs (Opus, VP8): simulcast layer
selection keys off the RID in RTP headers, BWE keys off TWCC packet
sizes/timing, PLI/FIR ride the RTCP control plane, and the egress
packetizer regenerates VP8 payload descriptors rather than copying them
— all of that keeps working on ciphertext. The single payload-derived
value in the whole SFU path is VP8 keyframe detection
(`Media::Data::is_keyframe()`), which str0m uses only to stamp metadata
that huddle consumes **only** in the RTP recorder — and E2EE meetings
disable server-side recording anyway (product decision below). Measured
crypto cost is ~11–15 µs/frame with WebCrypto AES-GCM-128 in a worker and
+16 bytes/frame of tag overhead, both negligible against a 33 ms frame
budget. The known warts are manageable: ciphertext corrupts SFU-side
keyframe detection (harmless once the recorder is off; senders must honor
PLI unconditionally instead of "on keyframe loss"), and receiver-side
frame counters cannot be trusted for IVs (solved by deriving the IV from
frame metadata or an SFrame in-band counter).

**Fallback if the product later rejects the constraints**: ship the
two-layer design only as a per-meeting trust mode ("E2EE meeting"),
keeping plaintext meetings as the default. No code is shared that would
make plaintext mode impossible; the transform worker is only attached
when the meeting is flagged E2EE.

## Browser support floor

Data from MDN browser-compat-data and caniuse (wf-webrtc-encoded-transform),
fetched 2026-10-20; local rows from a probe against the pinned e2e Chrome
(chrome-headless-shell 155.0.8059.39, Linux, `--use-fake-device-for-media-stream`).

| Capability | Chrome/Edge | Firefox | Safari |
|---|---|---|---|
| `RTCRtpScriptTransform` + `sender/receiver.transform` | 141 | 117 | 15.4 |
| Transform applied to **audio** frames | 86 (since legacy) / 141 (new API) | 117 | 15.4 |
| Transform applied to **video** frames | 86 / 141 | 117 | 15.4 |
| `RTCEncodedVideoFrame()` constructor | 127 | 145 | 26 |
| `RTCEncodedAudioFrame()` constructor | 127 | 145 | 26 |
| Full `getMetadata()` (seq/ssrc/payloadType/timestamp) | 127 | 117 (partial: payloadType+ssrc; `mimeType` still absent) | 15.4 |
| Legacy `createEncodedStreams` | 86 (present through 155) | never | never |

Notes on the table:

- The issue's premise about Firefox audio lagging is outdated as stated:
  audio frames work in Firefox from its first transform release (117).
  What is genuinely later in Firefox: the frame constructors (145) and
  the richer metadata (the audio transform initially exposed only
  `payloadType` + `synchronizationSource`; `mimeType` remains unimplemented
  there per BCD as of 2026-10). We must not depend on `mimeType` metadata
  — we already know our codecs statically (Opus + VP8 only).
- The W3C spec places the `rtctransform` event in the **Worker only**;
  Chromium additionally tolerated main-thread transforms (that is what
  Google Meet uses), but we design for the spec shape: all crypto lives in
  the worker. Local probe confirms: `typeof onrtctransform` is
  `undefined` on `window` in Chrome 155 — the event is worker-only, and
  the transform object must be constructed inside the worker and assigned
  from there.
- Legacy `createEncodedStreams` (Chromium-only, mid-2020, used by Google
  Duo and Jitsi pre-2023) is present in Chrome 155 but deprecated and
  non-portable. **We do not implement it**; the floor is the standard API
  (Chrome 141+, reached stable Oct 2025). Chromium-only environments that
  need older coverage are out of scope for a spike-derived first version.
- Chrome's `RTCRtpScriptTransformController` (an extra options surface
  from the spec) came back `undefined` in our 155 probe despite BCD
  claiming support — we do not rely on it either way.

Local probe facts (chrome-headless-shell 155, loopback call, fake A/V):

- Feature detects: `RTCRtpScriptTransform` = function,
  `RTCEncodedVideoFrame`/`RTCEncodedAudioFrame` = function,
  `'transform' in RTCRtpSender.prototype` and in
  `RTCRtpReceiver.prototype` = true, `createEncodedStreams` = function.
- A **full E2EE round trip** (both sides, VP8 video + Opus audio, AES-GCM
  128 in the worker) completed with **0 decrypt failures** across 103
  frames; ~1.5 s to first 20+ decoded frames on both sides.
- One `RTCRtpScriptTransform` instance per sender/receiver: sharing one
  instance across two senders throws "Transform cannot be reused".
  Design implication: one transform object per track, all tracks may
  share one worker.
- `getMetadata()` in Chrome 155 exposes `mimeType` ("video/VP8",
  "audio/opus"), `payloadType`, `rtpTimestamp`, `synchronizationSource`
  (video also `frameId`, `dependencies`, `width`/`height`,
  `spatialIndex`/`temporalIndex`; audio also `audioLevel`; receive side
  adds `receiveTime`/`sequenceNumber`). We can use `rtpTimestamp` + SSRC
  as an IV input on every engine that implements the standard API, but
  see the crypto section for why an in-band counter is still safer.

Recommended client gate (feature detection, matches MDN guidance):

```js
const supportsE2EE =
  typeof RTCRtpScriptTransform === "function" &&
  "transform" in RTCRtpSender.prototype &&
  typeof Worker !== "undefined";
```

Meetings flagged E2EE must be joinable only by clients that pass this
check plus the key-exchange requirements of #27.

## str0m 0.24.1 passthrough analysis

The question is whether the SFU's media pipeline breaks when frame
payloads are ciphertext. Walked the vendored source
(`str0m-0.24.1`), file by file:

- **Ingress depayloadizing** (`media/mod.rs`, `packet/vp8.rs`,
  `packet/opus.rs`): frames are reassembled from RTP packets. The VP8
  depacketizer parses the **RTP payload descriptor** (RFC 7741) — which
  is generated by the *browser's packetizer*, not derived from frame
  content — so reassembly works over ciphertext. The Opus depacketizer is
  fully content-agnostic. Depacketize errors only occur on truncated or
  malformed descriptors, which a real browser never produces.
- **Simulcast selection** (`Peer::forward_media`, `crates/sfu/src/peer.rs`
  around line 756): `wants_layer` matches `data.rid` against the
  subscriber's chosen layer, and `writer.match_params(data.params)`
  matches payload type/params. Both come from RTP headers and SDP, not
  payload bytes. Ciphertext is invisible here.
- **BWE** (`src/bwe/`): str0m's bandwidth estimation is TWCC-driven —
  `on_media_sent(payload_size, is_padding, now)` uses packet sizes and
  timing only. Content-agnostic; works on ciphertext. (The extra 16–28
  bytes per frame slightly increase the bitstream BWE must carry; frame
  budget absorbs it, see perf.)
- **Egress packetizing** (`Writer::write` → `Payloader`): the VP8
  packetizer **regenerates its own payload descriptors** (splitting at
  MTU, optional `picture_id` counter) rather than trusting the ingress
  descriptor bytes. Since we do not re-use the ingress descriptor, the SFU
  can packetize ciphertext it cannot parse. Opus egress is a passthrough.
  This is the crucial property: **str0m never needs to interpret payload
  bytes to forward them.**
- **PLI/FIR**: `Event::KeyframeRequest` arrives over RTCP (SRTCP on the
  wire), a purely control-plane signal. huddle relays receiver PLI back
  to the origin (`route_keyframe_request`, `room.rs` ~line 393; sender
  side `request_keyframe*`, `peer.rs` ~lines 795–853, bumping the
  `keyframes_requested` counter). None of this touches payload bytes.
- **Keyframes — the one break**: `Media::Data::is_keyframe()`
  (`media/event.rs` ~line 289, driven by `Vp8` codec extra) reads the
  **first payload byte after the descriptor** (VP8 payload header P bit,
  RFC 6386 §9.1, parsed in `vp8.rs` ~lines 743–755 and
  `detect_vp8_keyframe` ~line 403). Over ciphertext this byte is
  effectively random: single-packet frames flag ~50% of frames as
  keyframes, multi-packet frames false-positive with probability
  ≈ 1−2⁻ⁿ (any packet's byte with LSB 0 sticks the "keyframe" bit). In an
  E2EE meeting, the SFU's notion of "keyframe" is garbage. Consequences:
  - `route_media`'s non-contiguity FIR (`room.rs` ~line 359) fires on a
    wrong heuristic — mostly harmless (it asks for more keyframes), but
    senders must treat PLI/FIR as authoritative regardless of the SFU's
    keyframe belief.
  - The **recorder's** `keyframe` stamp (`room.rs` ~line 432) is wrong —
    irrelevant because E2EE meetings disable recording (below).
  - The standard mitigation, and what we adopt: **senders honor every
    PLI/FIR immediately** (generate a VP8 keyframe on request) and the
    receiver app tracks keyframe reception via its own decrypted-payload
    state, not via SFU signals. Optionally the sender's transform can
    leave the VP8 payload header (first 10 bytes of keyframes / 3 bytes
    of delta frames) **unencrypted** so SFUs can classify frames — the
    webrtc/samples E2EE convention; that leaks only per-frame frame-type
    and dimension bits (RFC 6386 §9.1) and restores SFU heuristics.
    huddle should adopt this partial-header-clear convention: it keeps
    `is_keyframe()` truthful without exposing content. (SFrame full
    encryption would not; see crypto section for the tradeoff.)

**Conclusion**: str0m is a passable opaque relay for Opus + VP8. Nothing
in the forwarding path decodes, rescales, or re-encodes media; the only
content-derived signal is keyframe classification, which the
partial-header-clear convention preserves.

## SFU payload-byte audit (huddle side)

Grep-audit of the workspace for code that reads or mutates media payload
bytes beyond handing `data.data` to str0m:

- `Room::route_media` (`crates/sfu/src/room.rs` ~line 355) forwards
  `data.data` opaquely to `Peer::forward_media`; nothing inspects it.
- `Peer::forward_media` (`peer.rs` ~line 756) filters by RID/params and
  writes `data.data.clone()` — opaque.
- The **only** consumer of payload-derived information in the entire
  server is `data.is_keyframe()` in `Room::record_frame` (`room.rs`
  ~line 432), used solely to stamp the recorder's frame metadata.
- There is **no audio-level detection, no VAD, no content analysis**
  anywhere in the SFU; mute is a signaling-driven gate
  (`Peer::set_mute`), not payload-derived.
- `Recorder`/`RecorderSink` (`recorder.rs`, `crates/server/src/recording.rs`)
  store depayloadized frames as-is into per-participant `.rtp` files for
  ffmpeg-side muxing.

**Recording interlock — product decision**: with E2EE the recorder would
capture ciphertext it can never decrypt, producing useless files while
advertising "Recording" to participants. Both alternatives (decrypt via
server-held keys = breaks E2EE; record ciphertext = useless artifacts) are
worse than not recording. **Decision: enabling E2EE on a meeting disables
server-side recording for that meeting, and a meeting with active
recording cannot switch E2EE on.** Rationale: recording is already a
per-meeting, prominently-indicated state (REC broadcast via
`ServerMessage::Recording { active }`, `Welcome.recording` flag, and
`Error::RecordingActive`), so the interlock is one flag comparison on
both sides plus clear UX copy: "E2EE meetings can't be recorded. Start a
regular meeting to record." Consent-wise this is also the honest choice:
the REC indicator tells participants the meeting is *not* E2EE whenever
recording is active, so the two trust modes are visibly exclusive rather
than silently degraded.

## Crypto pipeline

**Cipher**: AES-GCM-128 per encoded frame, executed inside the transform
`Worker` via WebCrypto (`crypto.subtle`). No WASM crypto is needed:
measured cost is ~10.7 µs/frame encrypt, ~14.7 µs/frame decrypt on this
VM (see perf), far under budget; WebCrypto also uses hardware AES-NI where
available. A WASM AES-GCM (e.g. a vendored Rust `aes-gcm` build) is the
documented fallback if some target lacks WebCrypto in workers — deferred
until measurements demand it.

**Framing/nonce — the design decision the probe forced**: the IV/nonce
must never repeat under a key, and the **receiver-side transform cannot
trust its own frame counter**. Locally verified failure mode: with a
per-side monotonic counter as the only IV input, audio (600/600) survived
but video failed 240/242 frames — the receiver transform saw 4 more
video frames than the sender encrypted (the browser's receiver path
re-emits/duplicates frames around keyframe gating and transform attach),
desynchronizing the counters. Two safe designs exist:

1. **SFrame (RFC 9605, June 2024)** — RECOMMENDED. Each frame gets a
   clear-text header (key id `KID` + counter `CTR`), the IV is
   `salt XOR CTR` with per-KID key+salt HKDF-derived from a base key.
   The counter travels in-band, so no receiver-side inference exists to
   break; KID gives cheap key rotation; the design is standardized and
   battle-tested (and its MLS integration in RFC 9605 §9 maps onto the
   membership epochs #27 will need). Cost: a few header bytes per frame
   (7+ for the recommended header forms) on top of the 16-byte tag.
2. **Metadata-derived IV** (Jitsi-style): IV from
   `(SSRC, rtpTimestamp)` read out of `getMetadata()`. Probe-proven to
   work (attempt 3: 0 failures), zero header bytes beyond the tag. Risks:
   `rtpTimestamp` wraps (~26.5 h at 90 kHz — resync needed), and two
   frames from the same SSRC could share an rtpTimestamp in pathological
   re-emission — small but the SFrame counter is simply safer.

We adopt **SFrame RFC 9605 framing** with ciphersuite
`AES_GCM_128_FULL_TAG` (no tag truncation: 12 bytes of header + 16 tag
per frame ≈ +28 B/frame, matching Jitsi's measured overhead). The probe's
metadata-IV result stays in the ADR as the documented low-overhead
alternative and as the proof that both sides agree on metadata.

**Header-clear convention** (webrtc/samples, Jitsi, and Meet all do
this): leave the codec's payload header unencrypted — VP8: first 10
bytes of keyframes / 3 bytes of delta frames (RFC 6386 §9.1: P-bit frame
type, dimensions, token-partition info); Opus: the TOC byte (RFC 6716
§3.1). This keeps SFU keyframe classification honest (see str0m section)
at the cost of revealing frame type/size-class metadata. It is compatible
with SFrame (encrypt a suffix of the frame, header stays clear) and is
the community default. We accept this leak for v1; full-payload SFrame
can be a per-meeting hardening flag later.

**Keys**: one base key per meeting epoch; KID selects the current epoch
key. The SFU relays the (per-epoch, member-scoped) key-exchange messages
of #27 but never the key material; on join/leave the meeting advances an
epoch and rekeys, giving forward-secrecy per membership change (leavers
cannot decrypt future epochs; newcomers cannot decrypt past epochs).
Rekeying cadence beyond membership events: rotate after 2²⁴ frames under
one key (~4.6 h at 30 fps video; the SFrame CTR field bounds usage) or
every 24 h, whichever first — cheap because the CTR/KID header makes
rollover seamless. Per #27's scope: key distribution mechanism
(Diffie-Hellman tree vs. sender-keys vs. MLS) is decided there; this ADR
only fixes the *frame format and cipher* the exchange must provision.

**Worker wiring**: one dedicated `e2ee-worker.js` (created from a
same-origin URL, see implementation plan) handles all tracks; each
sender/receiver gets its own `RTCRtpScriptTransform` (instances are not
reusable — probe-verified) constructed in the worker; keys arrive via
`postMessage` from the Leptos app, frames never leave the worker.

## Performance

- **Measured locally** (this VM, chrome-headless-shell 155, fake 640×480
  cam + fake mic, SubtleCrypto AES-GCM-128, transform worker): encrypt
  ≈ **10.7 µs/frame** (1.1 ms over 103 frames), decrypt ≈ **14.7
  µs/frame** (1.5 ms over 102). Overhead **+16 B/frame** (GCM tag) in the
  metadata-IV probe; SFrame adds ≈ +12 B/frame (header) for ~+28 B/frame
  total. At 30 fps that is ~0.05% of one core for crypto and <0.1%
  bandwidth overhead — negligible in both axes.
- **Jitsi's published numbers** (lib-jitsi-meet E2EE, insertable streams +
  SFrame-variant, AES-GCM-128 in a Web Worker): **~5–10% CPU increase**
  for encrypt/decrypt on the client, **+28 B/frame**, **<1 ms/frame**
  worker latency. Same order as ours.
- **Low-end/SwiftShader-class hardware**: this VM is itself a
  software-rendered, no-hardware-AES-class environment and crypto was
  still ~15 µs/frame; the expensive part of E2EE is not AES — it is frame
  plumbing. Keeping the transform **entirely inside the worker**
  (streams are transferable; do not bounce frames main↔worker per frame)
  avoids the main-thread jank that historically plagued worker-based
  transforms (fixed in Chromium ~2022). On machines as weak as the
  ADR 007 blur worst case (224 ms/frame segmentation) the *crypto* alone
  would remain fine; a degrade path is nevertheless warranted: if the
  worker's frame queue backs up (measured EMA of per-frame transform
  time > 20 ms for 30 consecutive frames), surface "E2EE is struggling
  on this device" and offer the participant a plaintext rejoin — same
  pattern as the blur degrade.
- **Bandwidth/BWE**: +28 B/frame at 30 fps ≈ +840 B/s ≈ +6.7 kbps on
  video — inside the noise of the SFU's TWCC-driven BWE, which simply
  accounts for it.

## Implementation plan

Ordered for #27 (key exchange) then #28 (E2EE chat), on top of this
branch:

1. **#27 — key exchange and meeting epochs** (protocol + server + web):
   - `crates/protocol`: additive `PROTOCOL_VERSION`-compatible messages:
     a `MeetingE2ee { enabled, epoch }` flag on `Welcome` (additive
     field, serde default), and a generic relayed `E2eeKeying { epoch,
     payload }` client→server→client message (server treats `payload`
     as opaque bytes — it must stay codec-agnostic to avoid becoming a
     crypto dependency). No existing variant changes.
   - `crates/server`: meeting-level trust-mode flag (stored on the
     meeting row; E2EE set at creation for v1); enforce the
     **recording interlock** (reject `Recording` start when E2EE, reject
     E2EE when `Recording` active — reuse `Error::RecordingActive`
     pattern); rate-limit the new relay path like chat.
   - `crates/web`: E2EE handshake state (who contributes key material,
     epoch bookkeeping), exposed to the worker via `postMessage`.
2. **#28 — media transform + E2EE chat** (web-centric):
   - New asset `crates/web/assets/e2ee/e2ee-worker.js` (plain JS, no
     bundler — same pattern as ADR 007's vendored `blur.js`, loaded
     lazily, same-origin). Trunk already `copy-dir`s `assets`; the
     release pipeline's rust-embed picks `crates/web/dist` up
     automatically. Worker must be a **separate classic/ES script** (not
     a WASM bin) — trunk's `copy-dir` covers it with zero config.
   - CSP (`ui_csp`, `http.rs`): same-origin Worker scripts are already
     permitted because `worker-src` falls back to `script-src 'self'`;
     `blob:` workers would NOT be. Load the worker by URL, not from a
     Blob. Verify in the e2e CSP-path run.
   - `crates/web/src/rtc`: attach transforms — sender side at
     `addTrack`-time equivalent (construct in worker, assign
     `sender.transform`), receiver side in the `ontrack` handler
     (`receiver.transform`); one transform instance per track, one
     worker per call. Gate on the feature-detect + meeting flag.
   - `crates/web/src/flow.rs`/`call.rs`: E2EE status surface (badge,
     key-verification emoji/words out-of-scope for v1), recording
     disabled copy.
   - **E2EE chat**: the same meeting base key (HKDF with a distinct info
     string, per SFrame's multiple-context guidance) encrypts chat
     bodies client-side; `chat_messages` stores ciphertext and the
     history replay (HISTORY_LIMIT=50) works unchanged. Server-side
     keyword search would break — out of scope, none exists today.
3. **SFU changes**: none required for forwarding. Optional (post-v1):
   use the truthful keyframe bit (restored by the header-clear
   convention) to keep the recorder honest for plaintext meetings; E2EE
   meetings never record.

**Test strategy** (e2e can assert *opaque passthrough*, not decryption —
the browser's own decode is the oracle): existing ssrc-snapshot asserts
keep working (getStats is outside the transform); add: (a) an e2e knob
`?e2ee=1` (mirroring `?test=1`/`?turn=relay`) enabling a test-mode
transform worker that only *tags* frames (+4 B), asserting media still
flows and inbound-rtp framesDecoded grows — proves the plumbing;
(b) with real crypto on, assert `window.__huddleE2eeStats` (decrypt
success/failure counters exposed under `?test=1`) shows 0 failures —
the receiver-side oracle; (c) one sans-io SFU test feeding mutated
"encrypted" payload bytes through `Room` and asserting byte-identical
forwarding — optional, the str0m source analysis above already covers
it, keep only if cheap.

## Consequences

**Positive**
- True E2EE media with the existing SFU topology, simulcast, BWE, and
  PLI flow all preserved; no str0m fork or patch needed.
- Standardized framing (SFrame RFC 9605) instead of bespoke nonce
  management; the probe documents the receiver-counter pitfall so #28
  does not rediscover it.
- Chat gets E2EE for free from the same epoch keys.

**Negative / accepted costs**
- E2EE meetings cannot be recorded — an explicit, indicated product
  tradeoff.
- Frame type metadata (keyframe/delta, dimensions class) leaks via the
  header-clear convention; acceptable for v1, hardening flag possible
  later.
- Browser floor rises: E2EE meetings require Chrome/Edge 141+, Firefox
  117+, Safari 15.4+; older Chromiums (which only have the deprecated
  legacy API) are excluded in v1.
- The SFU's keyframe heuristic is wrong under full encryption — mitigated
  by the header-clear convention and by senders honoring PLI/FIR
  unconditionally.

**Open questions for #27 / #28**
- Key-exchange mechanism inside the epoch model (sender-keys à la
  SFrame §9 vs. a proper MLS tree) — #27's core decision; this ADR
  fixes only that whatever is chosen must provision per-epoch SFrame
  base keys.
- Whether `E2eeKeying` relay messages should be a new WS message type
  or reuse the chat path with an encrypted envelope — protocol-shape
  decision for #27 (both are additive-safe).
- Key-verification UX (safety-number style) — deferred, but the worker
  API should expose the current epoch's key fingerprint early so #28
  can surface it without rework.
- Trickle of the degrade path: define the worker-queue metric and the
  "struggling" copy with real low-end hardware in #28's e2e (the
  `?test=1` stats hooks make this scriptable).
