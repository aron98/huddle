# ADR 002: Web-first Rust/WASM client with Leptos; Tauri desktop later

- Status: accepted
- Date: 2026-10-13

## Context

The client must be Rust, modern, and lightweight. Key constraint: browsers do not
expose raw RTP — the client's media stack is always the browser's
`RTCPeerConnection`, driven through `web-sys`, regardless of language. So a native
Rust client (egui/iced + webrtc-rs) would not share any media code with the web
client; it would re-implement the entire stack. Framework candidates for the WASM
app: Leptos, Dioxus, Yew. Native-only clients (egui, iced) were also considered.

## Decision

- Primary client: Rust compiled to WASM, UI in **Leptos**.
- Keep all WebRTC interop in a thin `RtcClient` module that Leptos components
  consume via signals — the interop layer stays targetable by headless-Chrome E2E tests.
- Desktop: post-MVP **Tauri** shell reusing the same web UI.
- Native Rust clients are explicitly out of scope.

## Consequences

**Positive**
- One UI codebase serves web + desktop (Tauri) + PWA installability for mobile.
- Browser media stack for free: codecs, AEC, camera/screen APIs, device handling.
- Leptos: fine-grained reactivity, small bundles (<300 KB gz budget is
  comfortably achievable), SSR available for the scheduling pages later.

**Negative**
- Two-language friction at the FFI edge (`web-sys` interop) — contained in `RtcClient`.
- Leptos is younger than Yew: fewer off-the-shelf component examples; acceptable
  given our small custom UI surface.
- Leptos↔async/WS boundary needs care (signals updated from WS callbacks).

**Rejected alternatives**
- *Dioxus*: strong contender (React-like DX, desktop targets); chosen second.
  Migration cost later is low since UI is a thin layer over `RtcClient`.
- *Yew*: oldest ecosystem but heavier reactivity model.
- *egui/iced native*: doubles the media-stack surface for desktop that almost
  nobody in the self-host audience needs; rejected.

## Amendment (2026-10-14)

- The post-MVP Tauri shell is tracked in
  [#18](https://github.com/aron98/huddle/issues/18).

## Amendment (2026-10-21)

- The desktop shell ships as **Electron**, not Tauri — system webviews break
  WebRTC/screen-share/E2EE parity on Linux and macOS. See
  [ADR 014](014-desktop-shell-electron.md), which supersedes the "Tauri
  desktop later" line above.
