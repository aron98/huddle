# ADR 007: Background blur via canvas compositing and replaceTrack

- Status: accepted
- Date: 2026-10-20

## Context

Issue #32 asks for background blur on the local camera feed, toggleable in
prejoin and mid-call, with a documented degrade path when the machine cannot
keep up. The constraints that shape the design:

- The web client is a Leptos CSR app built by trunk; there is no JS bundler,
  no worker story, and no room for a Node build step beyond what trunk
  already does.
- Toggling blur must not disturb the call: no renegotiation, no mute blip,
  no change to the simulcast setup (`high`/`low` rids).
- The runtime cost of segmentation is machine-dependent and must never
  freeze the meeting; a slow machine has to fall back to raw video on its
  own.

## Decision

1. **Pipeline**: per-frame canvas compositing on the main thread. A hidden
   `<video>` feeds the raw camera track; each captured frame runs through
   the segmenter; the compositor draws a blurred background plus the sharp
   person mask; `canvas.captureStream()` produces the processed track. The
   loop is driven by `requestVideoFrameCallback` (with a 33ms `setTimeout`
   fallback), drops frames when the previous one is still pending, and
   black-fills while the raw track is disabled so the camera-off toggle
   keeps working.
2. **Track swap**: `RTCRtpSender.replaceTrack` on the existing camera
   sender. The m-line, its simulcast rids, and the msid all stay; the
   remote side sees the same SSRCs continue to decode — verified by the
   e2e suite asserting ssrc-identical decode growth across on/off toggles.
3. **Segmentation**: vendored legacy
   `@mediapipe/selfie_segmentation@0.1.1675465747`, landscape model
   (`modelSelection: 1`, 144×256), SIMD-only WASM. The tasks-vision JS API
   dropped the dedicated selfie segmenter (only the 16MB multiclass image
   segmenter remains), so the legacy solution is both the lightest and the
   best-quality option. Everything lives in `crates/web/assets/effects/`
   (~6.3MB incl. the 5.7MB WASM), served same-origin via a trunk
   `copy-dir`, loaded lazily by injecting `blur.js` only on the first
   toggle. The CSP already allows same-origin scripts and
   `WebAssembly.instantiate`.
4. **Rust side**: `huddle-web`'s `effects` module owns a persistent
   "outgoing" stream (raw audio + current video track), swaps tracks in
   place, persists the preference in `localStorage` (`huddle-blur`), and
   exposes `replace_camera_track` on the browser peer. The platform-neutral
   `PeerConnection` trait is untouched.
5. **Degrade**: the shim keeps an EMA (α=0.1) of per-frame
   segment+composite time. Above 25ms for 30 consecutive frames it calls
   back into Rust, which tears the pipeline down, restores the raw track,
   and shows "Background blur was turned off to keep video smooth." A model
   that fails to load auto-disables immediately with its own notice.

## Consequences

**Positive**
- Toggle cost is a `replaceTrack` — no SDP round trip, no keyframe storm,
  no simulcast re-setup; the remote party never sees a track change beyond
  pixels.
- The heavy assets never load for users who never touch the toggle.
- Vendored, same-origin, version-pinned assets make the load deterministic
  (no CDN flake, no CSP rework, release builds embed them via rust-embed).

**Negative**
- Main-thread compositing competes with the UI; accepted for now, the
  auto-disable bounds the damage. Moving to an OffscreenCanvas worker or
  WebCodecs is the natural follow-up if it shows up in profiles.
- SIMD-only WASM: browsers without WASM SIMD fall into the
  model-load-failure auto-disable path. All evergreen browsers have SIMD;
  the path is tested (`blur-fail` e2e knob).
- The legacy MediaPipe solution is frozen upstream; if it ever breaks in a
  future Chrome, the same auto-disable path protects calls, and the model
  can be swapped with a shim-local change.
- ~6.3MB added to the repo and (lazily) to release artifacts.

## Measured performance

On the e2e VM (chrome-headless-shell 155, software WebGL via SwiftShader,
`--disable-gpu` — a deliberate worst case):

- segmentation + composite: **~224ms/frame EMA** (`avgMs` from
  `__huddleEffectsStats`), far above the 25ms budget; real-time is not
  achievable under SwiftShader, which is exactly why the degrade path
  exists. Remote decode on the receiving peer kept growing on the same
  ssrcs throughout (the pipeline emits a lower frame rate, not a broken
  stream).
- The `blur-slow` knob (synthetic 60ms/frame) trips the 30-frame streak
  auto-disable within ~2s, confirmed by the e2e suite.
- The 144×256 landscape model is 249KB (tflite); the WASM runtime is
  5.7MB. On real GPU-backed Chrome the same solution is used by Google
  Meet-class products and runs comfortably inside the 25ms budget; we kept
  the budget conservative because segmentation is only one of several
  per-frame costs in the app.

## Test knobs (e2e only, `?test=1`)

- `blur-fail=1`: model init rejects → immediate auto-disable + notice.
- `blur-slow=1`: 60ms/frame delay → slow-frame auto-disable.
- `window.__huddleEffectsStats`: `{active, notice, error, framesIn,
  framesOut, avgMs, degraded}` — Rust owns the first three, the shim the
  rest.
