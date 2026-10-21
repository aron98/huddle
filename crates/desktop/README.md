# huddle desktop

Tauri 2 shell that wraps the existing huddle web client (issue #18, ADR 014).
This directory is a **standalone cargo workspace**, excluded from the root
workspace, because the Tauri dependency tree requires GUI system packages that
the server CI does not install.

The shell does not bundle the web app. It hosts a small home page (server URL +
join box); once connected, the window navigates to the huddle server origin,
which serves the real UI to the embedded browser exactly as it would to Chrome
or Firefox. Session cookies live in the webview's per-origin cookie jar, so
login is remembered per server with no extra plumbing.

## Layout

| Crate | Role |
| --- | --- |
| `core` (`huddle-desktop-core`) | Pure logic: server-URL validation, settings file, join-link parsing, meeting selection, tray-menu model, platform capability matrix. **No tauri dependency; unit-testable headlessly.** |
| `app` (`huddle-desktop-app`) | The Tauri shell: window, tray, IPC commands, HTTP client. Requires a GUI toolchain. |
| `ui` | The bundled home page (vanilla HTML/JS, `frontendDist`). |
| `app/icons` | Placeholder icons (script-generated). |

## Building from source

Prerequisites: Rust (toolchain per root `rust-toolchain.toml`), Node.js (for the
Tauri CLI), and the platform webview packages.

**Linux (Debian/Ubuntu)** system packages:

```sh
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

**Commands** (from this directory):

```sh
# headless: logic tests run without any GUI deps
cargo test -p huddle-desktop-core

# full build (needs the system packages above)
npx @tauri-apps/cli@^2 build
# debug run:
npx @tauri-apps/cli@^2 dev
```

Bundles land in `app/target/release/bundle/` (`.deb`, `.AppImage` on Linux;
the bundle target list is in `app/tauri.conf.json`). macOS and Windows builds
produce `.dmg`/`.msi` once those targets are added there — the current list is
Linux-only because that is what the release CI exercises first.

Release CI (`.github/workflows/tauri.yml`) builds bundles on `v*` tags only;
it has not produced an artifact until the first tag runs it.

## Settings

Stored as plain JSON at:

- Linux: `~/.config/app.huddle.desktop/settings.json`
- macOS: `~/Library/Application Support/app.huddle.desktop/settings.json`
- Windows: `%APPDATA%\app.huddle.desktop\settings.json`

Currently one field: `{"server_url": "https://meet.example.com"}`. Delete the
file to reset. Joining a meeting link from a different server persists that
link's origin as the configured server.

## Platform capability matrix

The embedded browser engine decides what works — see ADR 014 for sources.

| Capability | Linux (WebKitGTK) | macOS (WKWebView) | Windows (WebView2) |
| --- | --- | --- | --- |
| Camera/mic | Partial (embedder must enable media-stream + permission prompts) | Yes | Yes |
| Screen share | Partial (xdg-desktop-portal/PipeWire; fragile) | **No** (known wry/WebKit gap on Sonoma+) | Yes |
| Two-person calls | **No** (WebKitGTK lacks RTCPeerConnection until the 2.56 cycle, ~2027) | Yes | Yes |

Media APIs need a secure context: prefer HTTPS (huddle has built-in TLS).

## Manual verification checklist (required — cannot be done headless)

These acceptance items must be verified by a person on real hardware before
any release claim. Static review and unit tests do not cover them.

1. **Connect flow**: launch app → enter server URL → Test shows ok → Connect
   opens the huddle dashboard; relaunching the app remembers the server.
2. **Login**: request a magic link from the desktop shell's webview and
   complete sign-in; cookie persists across app restarts.
3. **Join**: paste a meeting link (and a bare code) in the home page and from
   the tray/Join box; the call view loads.
4. **Two-person call** (per platform, mark each): audio + video both ways,
   participant tiles, mute/camera toggles, screen share, screen-share
   reshare, leave → post-leave view.
5. **Tray**: tray icon appears; Open shows the window; Join next meeting
   opens dashboard (or joins when an API token exists); Settings shows the
   home page; window close hides to tray; Quit exits.
6. **Settings persistence**: corrupt the settings file → app still starts
   with defaults (file is replaced with defaults on next save).

Record per-platform results (OS version, engine version) with the release.

## Limitations

- Linux: two-person calls are not expected to work at all until WebKitGTK
  ships LibWebRTC (2.56 cycle); camera/screen-share are best-effort.
- macOS: screen share is a known gap in the embedded WKWebView on Sonoma+.
- Tray "Join next meeting" cannot authenticate: it opens the dashboard today
  (the session cookie is not readable outside the webview). See ADR 014.
- No auto-update channel; updates ship as new bundles.
