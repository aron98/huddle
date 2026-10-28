// Auto-update wiring for the desktop shell (issue #98).
//
// All decisions live in update-policy.mjs (pure, unit-tested there).
// This module is the thin main-process glue around them and cannot be
// exercised on a headless CI box — the update flow is part of the
// manual per-OS checklist in README.md.
//
//  - Windows/Linux (packaged): electron-updater against the GitHub
//    Releases of aron98/huddle, which release-desktop.yml already
//    publishes (installers + latest*.yml + blockmaps). Standard
//    behaviour: background download, install on quit (or restart when
//    the user accepts the prompt), differential updates where the
//    installer format allows. The deb package cannot self-update;
//    the AppImage does.
//  - macOS (packaged): notify-with-link only. Builds are
//    Developer ID-signed + notarized now (issue #103), so the
//    Squirrel.Mac prerequisite is met — but switching to real
//    self-update is a deliberate follow-up (it needs code + release
//    testing), so until then the shell detects new releases via the
//    GitHub API and hands off to the release page for manual download.
//  - Dev runs (app not packaged) never check for updates.

import { app, dialog, shell } from "electron";
import electronUpdater from "electron-updater"; // CJS: { autoUpdater }
import {
  autoUpdateMode,
  CHECK_INTERVAL_MS,
  GITHUB,
  hasDesktopAsset,
  isNewerVersion,
  latestReleaseApiUrl,
  nextNotifyState,
  parseLatestRelease,
  STARTUP_CHECK_DELAY_MS,
} from "./update-policy.mjs";

const { autoUpdater } = electronUpdater;

/**
 * Starts update checks for packaged builds. No-op in dev and on unknown
 * platforms. Failures are logged and swallowed — a dead update feed
 * must never disturb app usage.
 */
export function registerUpdater() {
  if (!app.isPackaged) return; // dev runs never phone home
  const mode = autoUpdateMode(process.platform);
  if (mode === "none") return;
  if (mode === "updater") setupAutoInstall();
  else setupNotifyLink(); // darwin, until a Developer ID signature exists
}

function schedule(check) {
  const tick = () => {
    Promise.resolve()
      .then(check)
      .catch((err) => console.warn(`update check failed: ${err?.message ?? err}`));
  };
  const first = setTimeout(tick, STARTUP_CHECK_DELAY_MS);
  const every = setInterval(tick, CHECK_INTERVAL_MS);
  // Update timers must never keep a quitting app alive.
  first.unref();
  every.unref();
}

function setupAutoInstall() {
  autoUpdater.setFeedURL({ provider: "github", owner: GITHUB.owner, repo: GITHUB.repo });
  autoUpdater.autoDownload = true; // standard electron-updater behaviour
  autoUpdater.autoInstallOnAppQuit = true;
  autoUpdater.on("error", (err) => console.warn(`update check failed: ${err?.message ?? err}`));
  autoUpdater.on("update-downloaded", async (info) => {
    const { response } = await dialog.showMessageBox({
      type: "info",
      title: "Huddle update ready",
      message: `Huddle ${info?.version ?? ""} has been downloaded.`,
      detail: "Restart now to install it, or it will be installed when you quit Huddle.",
      buttons: ["Restart now", "Later"],
      defaultId: 0,
      cancelId: 1,
    });
    if (response === 0) autoUpdater.quitAndInstall();
  });
  schedule(() => autoUpdater.checkForUpdates());
}

async function checkNotifyOnce(state) {
  const res = await fetch(latestReleaseApiUrl(), {
    headers: { Accept: "application/vnd.github+json", "User-Agent": "huddle-desktop" },
  });
  if (!res.ok) return state; // rate limit / offline: nothing to do
  const release = parseLatestRelease(await res.text());
  if (!release) return state;
  if (!isNewerVersion(app.getVersion(), release.version)) return state;
  if (!hasDesktopAsset(release, process.platform)) return state;
  const decision = nextNotifyState(state.lastNotifiedVersion, release);
  if (!decision.notify) return state;
  const { response } = await dialog.showMessageBox({
    type: "info",
    title: "Huddle update available",
    message: `Huddle ${release.version} is available.`,
    detail:
      "This app cannot update itself in place (see README — Auto-update). " +
      "Download the new version from the releases page and replace this app.",
    buttons: ["Download…", "Later"],
    defaultId: 0,
    cancelId: 1,
  });
  if (response === 0) shell.openExternal(release.url);
  return decision.state;
}

function setupNotifyLink() {
  let state = { lastNotifiedVersion: null };
  schedule(async () => {
    state = await checkNotifyOnce(state);
  });
}
