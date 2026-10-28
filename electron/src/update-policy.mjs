// Pure auto-update policy for the desktop shell (issue #98).
//
// Everything here is dependency-free so the tests pin the behaviour
// without Electron — same spirit as the fs-injected settings.mjs, but
// this module needs no fs at all. The main-process glue lives in
// updater.mjs; the split-platform rationale is in README.md ("Auto-update").
//
// Update source is the GitHub Releases feed that release-desktop.yml
// already publishes for every v* tag (huddle-<version>-<os>-<arch>
// installers plus electron-updater's latest*.yml metadata).

export const GITHUB = Object.freeze({ owner: "aron98", repo: "huddle" });

// First check runs shortly after startup so it never races window creation
// or the initial server load; then every few hours (issue #98: "check on
// startup + periodically"). updater.mjs unrefs both timers so they never
// keep the app alive at quit.
export const STARTUP_CHECK_DELAY_MS = 15_000;
export const CHECK_INTERVAL_MS = 4 * 60 * 60 * 1000;

/**
 * How this platform updates itself (issue #98):
 *  - "updater": electron-updater against GitHub Releases. Windows NSIS
 *    and the Linux AppImage support in-place self-update.
 *  - "notify": detect new releases via the GitHub API and link the
 *    release page. macOS builds were only ad-hoc signed when this was
 *    written; they are Developer ID-signed + notarized now (issue
 *    #103), but the switch to Squirrel.Mac self-update is a deliberate
 *    follow-up, so darwin keeps the notify flow until then.
 *  - "none": never update in place (unknown platforms; dev runs are
 *    filtered earlier by app.isPackaged in updater.mjs).
 *
 * @returns {"updater" | "notify" | "none"}
 */
export function autoUpdateMode(platform) {
  if (platform === "win32" || platform === "linux") return "updater";
  if (platform === "darwin") return "notify";
  return "none";
}

/** Strips the leading "v" of a release tag: "v1.2.3" → "1.2.3". */
export function stripLeadingV(tag) {
  return typeof tag === "string" && tag.startsWith("v") ? tag.slice(1) : tag;
}

/**
 * True when `candidate` is strictly newer than `current`. Plain numeric
 * dot-version compare; a trailing pre-release/build suffix (e.g.
 * "0.2.0-rc.1") is ignored so a pre-release tag never rolls anyone back.
 * Anything unparsable is never newer — a malformed feed must not trigger
 * updates.
 */
export function isNewerVersion(current, candidate) {
  const a = parseVersion(current);
  const b = parseVersion(candidate);
  if (!a || !b) return false;
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const diff = (a[i] ?? 0) - (b[i] ?? 0);
    if (diff !== 0) return diff < 0;
  }
  return false;
}

function parseVersion(version) {
  if (typeof version !== "string") return null;
  const bare = stripLeadingV(version.trim()).split(/[-+]/)[0];
  if (!/^\d+(\.\d+)*$/.test(bare)) return null;
  return bare.split(".").map((n) => Number.parseInt(n, 10));
}

/** GitHub REST endpoint for the latest published, non-prerelease release. */
export function latestReleaseApiUrl(owner = GITHUB.owner, repo = GITHUB.repo) {
  return `https://api.github.com/repos/${owner}/${repo}/releases/latest`;
}

/**
 * Parses the body of GET /repos/{owner}/{repo}/releases/latest into
 * { version, tag, url, assets }, or null when it is not a usable
 * release (bad JSON, missing tag, non-https release URL).
 *
 * @param {string} text raw response body
 * @returns {{ version: string, tag: string, url: string, assets: string[] } | null}
 */
export function parseLatestRelease(text) {
  let data;
  try {
    data = JSON.parse(typeof text === "string" ? text : "");
  } catch {
    return null;
  }
  const tag = data?.tag_name;
  const url = data?.html_url;
  if (typeof tag !== "string" || tag === "") return null;
  if (typeof url !== "string" || !/^https:\/\//.test(url)) return null;
  const assets = Array.isArray(data?.assets)
    ? data.assets
        .map((asset) => (typeof asset?.name === "string" ? asset.name : ""))
        .filter((name) => name !== "")
    : [];
  return { version: stripLeadingV(tag), tag, url, assets };
}

/**
 * True when the release actually carries a desktop installer for
 * `platform` — old tags may predate the desktop workflow and must not
 * trigger an update dialog with nothing to download.
 */
export function hasDesktopAsset(release, platform) {
  if (!release) return false;
  const wanted = { darwin: ".dmg", win32: ".exe", linux: ".AppImage" }[platform];
  if (!wanted) return false;
  return release.assets.some((name) => name.endsWith(wanted));
}

/**
 * Once-per-version notify state: the macOS "download it yourself" dialog
 * must not repeat on every periodic check after the user picks Later.
 * The state lives for the session (updater.mjs holds it); a Huddle
 * restart naturally re-notifies, which is the intended freshness.
 *
 * @param {string | null} lastNotifiedVersion
 * @param {{ version: string } | null} release
 * @returns {{ notify: boolean, lastNotifiedVersion: string | null }}
 */
export function nextNotifyState(lastNotifiedVersion, release) {
  if (!release || release.version === lastNotifiedVersion) {
    return { notify: false, lastNotifiedVersion: lastNotifiedVersion ?? null };
  }
  return { notify: true, lastNotifiedVersion: release.version };
}
