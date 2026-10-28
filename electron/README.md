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
| electron | **45.0.0** (exact) | `latest` dist-tag, 2026-10-21 |
| → Chromium | **156.0.8078.48** | ≥ 141 floor required by [ADR 009](../docs/adr/009-e2ee-media-encoded-transform.md) (E2EE media transforms) |
| → Node (bundled runtime) | 24.22.0 | what the main process runs on |
| electron-builder | **26.15.3** (exact) | bundling only, in CI |
| electron-updater | **6.8.11** (exact) | runtime self-update against GitHub Releases ([Auto-update](#auto-update-issue-98)) |

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
- `npm test` — `node --test` over the pure-logic tests (settings, deep
  links, load-error mapping, IPC-bridge wiring, update policy, icon/PNG
  invariants, package pins)
- `npm run icons` — regenerate `build/icon.png` / `build/trayTemplate.png`
- `npm run dist` — local electron-builder build (CI does the real one)

## Production build

Pushing a `v*` tag triggers [release-desktop.yml](../.github/workflows/release-desktop.yml):
a windows/ubuntu matrix plus a dedicated macOS job run `npm ci` +
`electron-builder --publish never` and attach
`huddle-<version>-<os>-<arch>.{exe,dmg,AppImage,deb}`
plus the electron-updater feed files (`latest.yml`, `latest-linux.yml`,
`latest-mac.yml`, `.blockmap` — see [Auto-update](#auto-update-issue-98))
to the GitHub release. electron-builder only writes those feed files
when a publish provider is configured, so `electron-builder.yml`
declares the GitHub provider — CI still passes `--publish never`, which
makes electron-builder generate the files locally without publishing
anything (the workflow does the uploading), and asserts each platform's
feed file exists before upload (a missing feed must fail the release,
not skip silently). **macOS builds are Developer ID-signed and notarized**
(issue #103) — the dmg launches on the first try with no Gatekeeper
warning, and the workflow verifies the signature, the notarization and
the stapled ticket before upload (see
[macOS signing](#macos-signing-issue-103)).
**Windows/Linux builds are unsigned** — expect a Windows SmartScreen
first-launch warning; this is disclosed in the release
notes.

### macOS signing (issue #103)

macOS builds are signed with the org's **Developer ID Application**
certificate and **notarized** with Apple. `electron-builder.yml`
deliberately omits `mac.identity` — electron-builder auto-detects the
sole Developer ID Application certificate in the temporary keychain
the release workflow prepares (an explicit
`Developer ID Application: <CN>` value is rejected by its findIdentity;
`-` ad-hoc and `null` no-sign were the pre-notarization states) — and
sets `notarize: true` (notarytool submission; @electron/notarize
staples the ticket automatically). electron-builder 26.15.3 only
*warns* if notarization credentials are missing, so the workflow
enforces the result before upload: `codesign --verify --deep --strict`
(seal tree intact), Gatekeeper's assessment `spctl --assess`
(rejects non-notarized Developer ID apps on 10.15+),
`xcrun stapler validate` (stapled ticket), and the same spctl + stapler
re-run on the app inside the mounted dmg — the shipped artifact; the
dmg itself is unsigned by design (`dmg.sign` defaults to false).

The signing + notarization secrets live as **environment secrets of
the `macos-signing` GitHub environment** — never repo-level secrets —
so only the macOS release job (`bundle-macos`) can read them.

**One-time maintainer setup** (already done for this org; recorded
here so it can be recreated):

1. GitHub → repo **Settings → Environments → New environment** → name
   it `macos-signing`.
2. Add the four **environment** secrets:
   - `MAC_CERTS_P12_BASE64` — base64 of the exported Developer ID
     Application `.p12`;
   - `MAC_CERTS_PASSWORD` — that `.p12`'s export password;
   - `APPLE_ID` — the Apple ID email;
   - `APPLE_APP_SPECIFIC_PASSWORD` — an app-specific password
     (appleid.apple.com → Sign-In and Security → App-Specific
     Passwords); the notarization API rejects primary passwords.
3. Nothing else: the workflow imports the certificate into a
   temporary keychain per run, derives `APPLE_TEAM_ID` from the
   imported certificate (notarization requires it), and deletes the
   keychain afterwards.

Notes:

- The Developer ID Application certificate must be created by the
  team's Account Holder (developer.apple.com → Certificates).
- electron-builder skips macOS code signing on **pull request** builds
  (unless `CSC_FOR_PULL_REQUEST` is set) — harmless here, the release
  workflow only runs on `v*` tags.
- **Old pre-notarization builds only** (ad-hoc signed, released before
  this change): first launch still triggers Gatekeeper's "Apple cannot
  check it for malicious software" warning. Override it once for those
  versions: right-click `Huddle.app` → **Open** → **Open**, or System
  Settings → Privacy & Security → "Open Anyway", or strip the
  quarantine flag directly:
  `xattr -rd com.apple.quarantine /Applications/Huddle.app`.

## Auto-update (issue #98)

Packaged builds check the GitHub Releases of this repository for a newer
version 15 seconds after startup and then every 4 hours. `npm run dev`
and other unpackaged runs never check. All failures — offline, DNS,
GitHub rate limits, a release predating the desktop workflow — are
logged and swallowed: a dead feed never disturbs app usage. The decision
core is the pure `src/update-policy.mjs` (unit-tested); the main-process
glue is `src/updater.mjs`.

The update behaviour is **split by platform**, because self-update
mechanisms differ:

- **Windows (NSIS)** — [electron-updater](https://www.electron.build/auto-update)
  downloads the new installer in the background (differentially, via the
  published `.blockmap`), then shows an "update ready" dialog: **Restart
  now** installs immediately, **Later** installs on quit.
- **Linux** — the same flow, but only the **AppImage** can replace
  itself; a **deb** install cannot self-update (electron-updater has no
  deb path) and stays silently on its version. The AppImage is the
  recommended desktop format.
- **macOS** — **notify-with-link only, no self-update.** electron-updater
  on macOS drives Squirrel.Mac, which requires a properly **Developer
  ID-signed** build; ours was only ad-hoc signed when this was written
  (issue #86), and a self-replace would have broken the app. Instead
  the shell polls the GitHub API for the latest release and, when a
  newer version with a `dmg` asset exists, shows an "update available"
  dialog whose **Download…** button opens the release page in the
  browser. The dialog appears once per version per session (picking
  Later silences periodic re-checks of the same version). Builds are
  Developer ID-signed + notarized now (issue #103), so the Squirrel.Mac
  prerequisite is finally met — but switching macOS to a real
  self-update flow is a deliberate follow-up (it needs code + release
  testing), not something the signing change flips.

The Windows/Linux feed is the `latest.yml` (NSIS) / `latest-linux.yml`
(AppImage) metadata plus blockmaps that `release-desktop.yml` attaches
to every `v*` release — the presence of those files is pinned by a
`package.test.mjs` invariant. Updater versions are exactly pinned in
`package.json` (`electron-updater` 6.8.11) and the feed repo/owner is
`GITHUB` in `src/update-policy.mjs`.

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
  `settings.json` under Electron's `userData` dir. A Connect that cannot
  load the server shows the mapped net error inline on the form; an
  unreachable saved server falls back to the connect page with the error
  instead of a dead tab.
- Self-update from GitHub Releases on Windows/Linux; macOS notifies with
  a download link — see [Auto-update](#auto-update-issue-98).

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
   address shows the inline error; an unreachable address (server down,
   bad hostname) shows the mapped net error inline with the raw `ERR_*`
   code; a good address connects and persists
   (restart goes straight to the server).
6. Kill the server, reload: connect page returns with the error message.

**Update flow (issue #98, packaged builds only)** — needs an installed
build at version A and a published `v*` release at a higher version B
(whose release carries the `latest*.yml` feed files):
1. Start the app with the network blocked: it starts, connects to the
   server, and never shows an update dialog (checks fail silently).
2. Restore the network and relaunch: the first check fires ~15s after
   startup; later checks run every 4h.
3. **Windows**: "Huddle update ready" dialog appears after the background
   download. Restart now → relaunches on B (verify in
   `window.huddleDesktop.info()` / install dir); Later → quit and
   relaunch confirms the install-on-quit path.
4. **Linux**: same dialog on the **AppImage** install; the relaunched
   app is the new AppImage. A **deb** install shows no update prompt —
   expected (deb cannot self-update).
5. **macOS**: "Huddle update available" dialog appears; Download… opens
   the release page in the browser; Later → the dialog does not repeat
   for the same version in the same session (relaunch re-shows it once).

**Windows** — "Share computer audio" checkbox visible in the picker; checked
→ shared tab/system audio audible to a second participant.
**macOS** — same audio checkbox (13.3+); Dock activate reopens the window;
no tray is created (by design); the dmg build launches on the first try
with **no Gatekeeper warning** (Developer ID-signed + notarized — see
[macOS signing](#macos-signing-issue-103); a warning means the signing
or notarization regressioned — do not ship).
**Linux** — no audio checkbox; tray icon appears on a DE with a tray
(close → hides to tray, Show → restores); on a tray-less DE the app must
still start and close normally.

**Verified by the author:** items 2 (feature-detect constants), pure-logic
tests, syntax gate, icon generation, PNG invariants — all on Linux with the
bundled versions above.
**NOT verified (no hardware/OS access in this environment):** packaged
(installer) runs on any OS, the auto-update flow on any OS (electron-updater
against a real GitHub release, the macOS notify dialog), actual media flow
through the shell, loopback audio on Windows/macOS, deep-link registration
on installed builds, tray behaviour on real DEs, dmg/exe/deb install paths,
and the macOS Developer ID signature + notarization itself — config-level
only on this VM (no macOS, no keychain, no secrets); the
`verify Developer ID signature, notarization and the dmg` step in
release-desktop.yml performs the real check on the next tagged release.
