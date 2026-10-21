# ADR 014: Tauri 2 desktop shell reusing the web client

- Status: accepted
- Date: 2026-10-21

## Context

ADR 002 deferred a desktop client to post-MVP and chose the Leptos web client
partly so a Tauri shell could reuse it. Issue #18 asks for that shell. The
constraints that shaped this ADR:

1. The desktop crate must not destabilize the root workspace — its
   `tauri`/`wry` dependency tree cannot build on the VMs that run the four
   root quality gates, and a Tauri build requires webkit2gtk system packages
   that CI for the server does not (and should not) install.
2. The web client is strictly same-origin (relative-path REST in
   `crates/web/src/api.rs`, signaling WS from `window().location()`), and the
   server embeds the compiled UI via rust-embed (ADR 004) with no CORS layer.
3. Every platform embeds a different browser engine, and huddle's whole feature
   set (getUserMedia, getDisplayMedia, RTCPeerConnection) lives in that engine.
   The engine matrix — not the Rust code — decides what the shell can actually
   do per platform.

## Decision

- Wrap the existing web client in **Tauri 2** (`crates/desktop`), a
   **standalone cargo workspace excluded from the root workspace**, split into:
  - `core` (`huddle-desktop-core`): pure, tauri-free logic — server-URL
    parsing/validation, settings persistence, join-link handling, meeting
    selection, tray-menu model, capability matrix. Unit-testable headlessly.
  - `app` (`huddle-desktop-app`): the thin Tauri shell (window, tray, commands,
    plugin-http client). Requires a GUI toolchain to build.
- **UI serving: point the webview at the huddle server origin.** The shell does
  not bundle or proxy the Leptos app. `frontendDist` holds only the shell's
  own home page (server URL field + join box); after connecting, the main
  window navigates to the server's origin, where the server serves the real UI
  exactly as to a browser. The session cookie lives in the webview's per-origin
  cookie jar, so "remembers auth per server" works with zero cookie plumbing.
- **CSP is left null** (`security.csp` in `tauri.conf.json`): the remote page
  is the server's own UI; injecting a strict policy into it risks breaking it.
- **No Tauri capability is granted to remote content.**
  `capabilities/default.json` is `local: true`, windows `["main"]`,
  permissions `core:default` — server pages get zero Tauri IPC; only the
  bundled home page can invoke commands.
- Tray "Join next meeting" is honest about auth: the shell's HTTP client has
  no session credentials (the cookie is in the webview jar and not readable by
  the shell), so `/api/meetings/mine` is expected to return 401 today; in that
  case the action opens the dashboard in the webview instead. The pure
  selection logic (`next_join_target`) is ready for a future API token.

## Consequences

**Positive**
- Zero changes to `crates/web` and no CORS anywhere: same-origin fetch, cookie
  auth, and the embedded-UI server all keep working untouched. Any UI feature
  shipped to the server appears in the desktop shell with no rebuild.
- `core` runs under plain `cargo test` in CI without any GUI system deps, so
  the shell's logic is continuously verified even though a full Tauri build is
  not part of regular CI.
- Root workspace gates stay fast and green; desktop never enters them
  (`exclude = ["crates/desktop"]`).

**Negative**
- Desktop bundles cannot be produced on a headless VM; they are built only in
  the tagged-release CI job and require manual verification (see
  `crates/desktop/README.md`).
- The shell cannot read the webview session, so tray actions are limited to
  what the unauthenticated API allows; "Join next meeting" degrades to
  "open dashboard" until an API token exists.
- Platform engine gaps (below) mean Linux desktop is a second-class citizen
  for calls for now.

## Platform capability matrix

What the embedded engine gives us per platform (sources: WPE WebKit FAQ,
WebKitGTK 2.50/2.56 release notes and bug 290841, Igalia WebKit periodical
#64; tauri-apps/wry #1101, tauri-apps/tauri #8979, WebKit bug 274896;
Microsoft Learn ICoreWebView2_27 screen-capture events; MDN secure-context
requirements for media APIs):

| Capability | Linux (WebKitGTK) | macOS (WKWebView) | Windows (WebView2) |
| --- | --- | --- | --- |
| Camera/mic (getUserMedia) | Partial — requires `enable-media-stream` + permission-request handling | Yes — requires `requestMediaCapturePermissionFor` delegate + usage descriptions | Yes |
| Screen share (getDisplayMedia) | Partial — xdg-desktop-portal/PipeWire; fragile | No — wry cannot show the system prompt on Sonoma+ (known WebKit/wry gap) | Yes — OS-level picker |
| RTCPeerConnection (calls) | No — LibWebRTC lands in the 2.56 cycle (~2027); stable tarballs lack it | Yes | Yes |
| Two-person call expected | No | Yes (manual verify) | Yes (manual verify) |

Notes: on Linux the embedder must set the `enable-media-stream` WebKit setting
and handle the permission-request signal (auto-denied by default); on macOS
missing Info.plist usage descriptions make TCC kill the app on capture start.
Media APIs generally require a secure context — run the server behind TLS
(huddle has built-in TLS termination, ADR 008).

## Deviations from the original issue sketch (all deliberate)

- The brief suggested `frontendDist` pointing at `crates/web/dist`; rejected:
  the web client fetches relative URLs and derives the WS URL from
  `window.location`, so serving it from the Tauri origin would break against
  a remote API (and CORS would need server changes). Navigating to the server
  origin needs zero web-client changes and no trunk build in desktop CI.
- A virtual manifest with two members instead of an "empty workspace table":
  a virtual manifest still needs its members listed to host the two inner
  crates; the intent (standalone, not in the root build) is fully met.
- Window close hides to tray instead of quitting (tray Quit exits).
- Icons are generated placeholder art (script-produced PNG/ICO/ICNS), not a
  designed logo.

## Verification status

- Verified on a headless VM: `huddle-desktop-core` unit tests, formatting,
  clippy on the pure crate.
- Requires manual pass on real hardware (checklist in `crates/desktop/README.md`):
  connecting, magic-link login, two-person call, screen share per platform,
  tray behavior. Do not assume acceptance items pass from static review alone.
