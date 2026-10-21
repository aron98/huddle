# huddle desktop shell

Electron shell that runs the Huddle meeting client as a desktop app. The
client is **not** bundled into the shell: it is same-origin by design
(relative-path REST, scheme-matched `wss://`/`ws://` signaling derived
from the page origin), so the shell loads the HTTP(S) origin of a Huddle
server, which serves the identical
rust-embed'd web client it serves to browsers ([ADR 004](../docs/adr/004-single-binary-embedded-ui.md)).
Rationale — bundled Chromium over Tauri's per-OS system webviews — is
[ADR 014](../docs/adr/014-desktop-shell-electron.md).

## Pinned toolchain

| Component | Version | Notes |
|---|---|---|
| Electron | **45.0.0** (exact) | `latest` dist-tag, 2026-10-21 |
| → Chromium | **156.0.8078.48** | ≥ 141 floor required by [ADR 009](../docs/adr/009-e2ee-media-encoded-transform.md) (E2EE media transforms) |
| → Node (bundled runtime) | 24.22.0 | what the main process runs on |
| electron-builder | **26.15.3** (exact) | bundling only, in CI |

Chromium/Node versions verified against
<https://releases.electronjs.org/releases.json> (canonical; the
`electron/releases` GitHub JSON is stale — do not cite it).

**Version policy:** bump deliberately — one Electron major at a time, record
the new Chromium version in this table, keep it ≥ 141 (the E2EE floor) while
E2EE is wired in the client. A Chromium security rollout is a good reason to
bump sooner than planned.

## Dev loop

```sh
# terminal 1 — the server with the web bundle (from the repo root)
(cd crates/web && trunk build)
HUDDLE_WEB_DIR=crates/web/dist cargo run -p huddle-server

# terminal 2 — the shell
cd electron
npm ci            # installs electron + electron-builder (dev only)
npm run dev       # --dev forces http://localhost:3000
```

`npm start` (without `--dev`) opens the configured server or the first-run
connect page. Other scripts:

- `npm run check` — `node --check` over every shell `.js`/`.mjs` (no Electron
  needed; runs in plain CI)
- `npm test` — `node --test` over the pure-logic tests (settings, deep links,
  icon/PNG invariants, package pins)
- `npm run icons` — regenerate `build/icon.png` / `build/trayTemplate.png`
- `npm run dist` — local electron-builder build (CI does the real one)

## Production build

Pushing a `v*` tag triggers [release-desktop.yml](../.github/workflows/release-desktop.yml):
a windows/macos/ubuntu matrix runs `npm ci` + `electron-builder --publish
never` and attaches `huddle-<version>-<os>-<arch>.{exe,dmg,AppImage,deb}` to
the GitHub release. **Builds are unsigned** — expect Windows SmartScreen and
macOS Gatekeeper warnings; this is disclosed in the release notes. macOS
builds skip code-sign probing via `CSC_IDENTITY_AUTO_DISCOVERY=false`.

## What the shell adds over a browser tab

- `huddle://` deep links (`huddle://meet.example.com/j/abc` → that server's
  join page); registered when packaged. Scheme for a link host: reuse the
  configured server's scheme when the host matches, else `http` for
  loopback and `https` for everything else.
- Custom screen-share picker over `desktopCapturer` (screens + windows).
  "Share computer audio" (loopback) is offered only where Electron supports
  it: Windows, macOS ≥ 13.3. On Linux the picker is video-only and
  `getDisplayMedia` resolves without an audio track — same as Chrome there.
- Tray icon with close-to-tray on Windows/Linux (guarded: DEs without a
  tray just lose the tray, not the app). macOS uses the Dock convention.
- First-run connect page; the chosen server origin is persisted to
  `settings.json` under Electron's `userData` dir. An unreachable server
  falls back to the connect page with the error instead of a dead tab.

## Manual verification checklist (per release, per OS)

Run the dev loop above, then verify by hand — this shell has no E2E suite;
CI only syntax-checks, unit-tests pure logic, and bundles.

**All platforms**
1. `npm ci && npm run dev` starts and shows the meeting UI (not a blank
   window) — proves preload + CSP interplay.
2. DevTools console (View → Toggle Developer Tools or Ctrl+Shift+I): run
   `await window.huddleDesktop.info()` → note the Chromium version; then the
   E2EE feature-detect:
   `typeof RTCRtpScriptTransform === "function" && "transform" in RTCRtpSender.prototype && typeof Worker !== "undefined"`
   must print `true` (the ≥ 141 floor is live in this build).
3. Join a meeting: camera preview, mic meter, in-call video tiles, mute
   toggle, chat.
4. Screen share: picker opens with screens + windows; picking one shares;
   Esc closes the picker and the client's share state resets (share
   button re-enabled).
5. First run with no `settings.json`: connect page appears; entering a bad
   address shows the inline error; a good address connects and persists
   (restart goes straight to the server).
6. Kill the server, reload: connect page returns with the error message.

**Windows** — "Share computer audio" checkbox visible in the picker; checked
→ shared tab/system audio audible to a second participant.
**macOS** — same audio checkbox (13.3+); Dock activate reopens the window;
no tray is created (by design).
**Linux** — no audio checkbox; tray icon appears on a DE with a tray
(close → hides to tray, Show → restores); on a tray-less DE the app must
still start and close normally.

**Verified by the author:** items 2 (feature-detect constants), pure-logic
tests, syntax gate, icon generation, PNG invariants — all on Linux with the
bundled versions above.
**NOT verified (no hardware/OS access in this environment):** packaged
(installer) runs on any OS, actual media flow through the shell, loopback
audio on Windows/macOS, deep-link registration on installed builds, tray
behaviour on real DEs, dmg/exe/deb install paths.
