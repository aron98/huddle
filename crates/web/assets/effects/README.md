# Vendored camera-effects assets

Files in this directory are served under `/assets/effects/` by the huddle
server (same-origin) and loaded lazily by `crates/web/src/effects.rs` when a
user first enables background blur.

| File | Provenance |
|---|---|
| `selfie_segmentation.js` | `@mediapipe/selfie_segmentation@0.1.1675465747` (npm), Apache-2.0 |
| `selfie_segmentation.binarypb` | same package — solution graph |
| `selfie_segmentation_landscape.tflite` | same package — landscape (144x256) segmentation model (`modelSelection: 1`) |
| `selfie_segmentation_solution_simd_wasm_bin.js` | same package — SIMD wasm loader |
| `selfie_segmentation_solution_simd_wasm_bin.wasm` | same package — SIMD wasm binary |
| `selfie_segmentation_solution_simd_wasm_bin.data` | same package — (empty) data segment |
| `blur.js` | huddle's own shim: loads the solution on demand, drives the per-frame compositing loop, exposes `window.__huddleBlur` |

Only the SIMD wasm variant is vendored. Browsers without wasm SIMD (all
evergreen browsers have it) fall back to the app's documented no-blur degrade
path rather than the non-SIMD binary.

The legacy Solutions API is used deliberately: the newer `@mediapipe/tasks-vision`
package no longer ships a dedicated selfie segmenter for the web, and the
multiclass image segmenter model it would require is an order of magnitude
heavier. See `docs/adr/007-background-blur-pipeline.md`.

License: Apache-2.0 (MediaPipe). `blur.js` is part of huddle.
