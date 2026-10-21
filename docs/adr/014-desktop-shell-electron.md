# ADR 014: Desktop shell on Electron, not a system webview

- Status: accepted
- Date: 2026-10-21

## Context

Issue [#18](https://github.com/aron98/huddle/issues/18) tracks the post-MVP
desktop shell. ADR 002 sketched it as a **Tauri** shell reusing the web client,
and a Tauri PR (#62) got as far as review before being rejected: Tauri uses the
OS webview, and the meeting client's requirements — WebRTC media, screen
sharing, and the E2EE insertable-streams pipeline of ADRs 009–011 — only line
up with Chromium-derived engines:

- **Linux (WebKitGTK)**: no usable WebRTC story until the WPE WebKit 2.48-era
  work lands (~2027 horizon); the SFU-dependent media path is effectively
  broken today.
- **macOS (WKWebView)**: `getDisplayMedia` is blocked on Sonoma+ for
  non-Safari-entitled binaries — screen sharing, a headline feature, is
  unimplementable.
- **Windows (WebView2)**: Chromium-based and fine — but 1 of 3 platforms is
  not parity.

The web client targets a **Chromium ≥ 141** floor (ADR 009: E2EE needs
`RTCRtpScriptTransform` + worker-based audio/video transforms, stable since
Chrome/Edge 141, Oct 2025). Bundling a known engine removes per-OS engine
roulette and keeps desktop on the same engine family the E2E suite tests.

The client is deliberately **same-origin** (relative-path REST, cookies,
`ws://<host>/ws` derived from the page origin). That rules out serving the
built `crates/web/dist` from a custom app scheme or `file://`: the shell would
be a second origin with no server to talk to.

## Decision

- The desktop shell is **Electron**, exact-pinned (`electron@45.0.0`,
  Chromium 156.0.8078.48, per https://releases.electronjs.org/releases.json).
- The shell **loads the Huddle server's HTTP origin** — the same rust-embed'd
  client every server serves (ADR 004). Dev mode points at
  `http://localhost:3000`; production asks for the user's server URL on
  first run (connect page) and persists it to `settings.json` in `userData`.
  This supersedes the issue's "production loads the built crates/web/dist"
  wording: same-origin is load-bearing.
- **Electron version policy**: bump deliberately, one release at a time, with
  the Chromium version recorded in `electron/README.md`; never fall below the
  Chromium 141 E2EE floor while E2EE is wired in the client.
- Native integrations are thin: `setDisplayMediaRequestHandler` with a custom
  picker (loopback audio only where Electron supports it: Windows, macOS
  ≥ 13.3), tray with close-to-tray on win32/linux, `huddle://` deep links.
- Bundling is CI-only on `v*` tags (mirroring the server release pipeline);
  builds are **unsigned** and the release notes disclose that.

## Consequences

**Positive**
- One engine across web and desktop: WebRTC, getDisplayMedia, and the E2EE
  transform pipeline behave exactly as in the supported browsers.
- Zero client code changes — the shell is a frame around the server's UI.
- Deep links (`huddle://meet.example.com/j/abc`) work with the same-origin
  client because they simply reconstruct the page URL.

**Negative / costs**
- Bundle size (~100 MB installed) and RAM overhead versus a system webview.
- An npm toolchain enters an otherwise Rust-only repo (contained in
  `electron/`, never a Cargo workspace member).
- Chromium security updates now require deliberate Electron bumps.
- Loading a remote origin once meant the shell inherited the client's
  `ws://` hard-coding (fixed since, issue #65: the client now derives
  `wss://` from the page protocol), which would have hit mixed-content
  blocking on https servers — the same-origin rule stands and both
  schemes now work.
- Unsigned binaries on Windows/macOS mean OS warnings; accepted for a
  self-hosted audience in v0.x.

**Rejected alternatives**
- *Tauri (system webviews)*: rejected pre-merge as above — engine parity.
  The never-merged Tauri PR's good ideas survive: an honest per-OS manual
  verification checklist and tag-only bundling CI.
- *Native Rust client*: rejected in ADR 002 already (no shared media code).
- *Serving `crates/web/dist` from the shell*: breaks same-origin REST/WS by
  construction.

## Supersedes

- ADR 002's "Desktop: post-MVP **Tauri** shell" line and issue #18's original
  Tauri framing.
