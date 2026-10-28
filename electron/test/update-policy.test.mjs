import assert from "node:assert/strict";
import { test } from "node:test";
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
  stripLeadingV,
} from "../src/update-policy.mjs";

// Release body shape of GET /repos/aron98/huddle/releases/latest with the
// fields the policy cares about (tag_name, html_url, assets[].name).
function releaseBody(overrides = {}) {
  return JSON.stringify({
    tag_name: "v0.2.0",
    html_url: "https://github.com/aron98/huddle/releases/tag/v0.2.0",
    assets: [
      { name: "huddle-0.2.0-win-x64.exe" },
      { name: "huddle-0.2.0-win-x64.exe.blockmap" },
      { name: "huddle-0.2.0-mac-arm64.dmg" },
      { name: "huddle-0.2.0-linux-x86_64.AppImage" },
    ],
    ...overrides,
  });
}

test("autoUpdateMode: Windows/Linux self-update, macOS notifies, unknown never", () => {
  // macOS keeps the notify-with-link path (issue #98) even though builds
  // are Developer ID-signed + notarized now (issue #103): switching
  // Squirrel.Mac on is a deliberate follow-up, not a signing side effect.
  assert.equal(autoUpdateMode("win32"), "updater");
  assert.equal(autoUpdateMode("linux"), "updater");
  assert.equal(autoUpdateMode("darwin"), "notify");
  assert.equal(autoUpdateMode("freebsd"), "none");
  assert.equal(autoUpdateMode(undefined), "none");
});

test("update schedule: delayed first check, hours between checks", () => {
  assert.ok(STARTUP_CHECK_DELAY_MS >= 10_000, "first check must not race startup");
  assert.ok(CHECK_INTERVAL_MS >= 3 * 60 * 60 * 1000, "checks every few hours");
  assert.ok(CHECK_INTERVAL_MS <= 24 * 60 * 60 * 1000, "at most daily");
});

test("stripLeadingV removes only one leading v", () => {
  assert.equal(stripLeadingV("v1.2.3"), "1.2.3");
  assert.equal(stripLeadingV("1.2.3"), "1.2.3");
  assert.equal(stripLeadingV("vv1"), "v1");
  assert.equal(stripLeadingV(42), 42);
  assert.equal(stripLeadingV(null), null);
});

test("isNewerVersion compares numeric dot segments", () => {
  assert.equal(isNewerVersion("0.1.0", "0.2.0"), true);
  assert.equal(isNewerVersion("0.1.0", "v0.2.0"), true, "release tags carry a leading v");
  assert.equal(isNewerVersion("0.1.0", "0.1.1"), true);
  assert.equal(isNewerVersion("0.1.0", "1.0.0"), true);
  assert.equal(isNewerVersion("1.2", "1.2.1"), true, "missing segments count as 0");
  assert.equal(isNewerVersion("0.1.0", "0.1.0"), false, "equal is not newer");
  assert.equal(isNewerVersion("0.2.0", "0.1.0"), false, "downgrade never");
  assert.equal(isNewerVersion("0.2.0", "0.2.0"), false);
});

test("isNewerVersion ignores pre-release/build suffixes and rejects junk", () => {
  // A pre-release tag is treated as its base version — never newer than
  // the same base, and never rolls back an installed rc.
  assert.equal(isNewerVersion("0.2.0", "0.2.0-rc.1"), false);
  assert.equal(isNewerVersion("0.1.9-rc.1", "0.2.0-rc.2"), true);
  assert.equal(isNewerVersion("0.1.0", "0.2.0+build.7"), true);

  for (const bad of [null, undefined, "", "not-a-version", "v", "0.1.x", {}]) {
    assert.equal(isNewerVersion("0.1.0", bad), false, JSON.stringify(bad));
    assert.equal(isNewerVersion(bad, "0.2.0"), false, JSON.stringify(bad));
  }
});

test("latestReleaseApiUrl points at the huddle repo's latest release", () => {
  assert.equal(latestReleaseApiUrl(), "https://api.github.com/repos/aron98/huddle/releases/latest");
  assert.equal(GITHUB.owner, "aron98");
  assert.equal(GITHUB.repo, "huddle");
  assert.equal(
    latestReleaseApiUrl("someone", "else"),
    "https://api.github.com/repos/someone/else/releases/latest",
  );
});

test("parseLatestRelease extracts version, url and asset names", () => {
  const release = parseLatestRelease(releaseBody());
  assert.equal(release.version, "0.2.0");
  assert.equal(release.tag, "v0.2.0");
  assert.equal(release.url, "https://github.com/aron98/huddle/releases/tag/v0.2.0");
  assert.deepEqual(release.assets, [
    "huddle-0.2.0-win-x64.exe",
    "huddle-0.2.0-win-x64.exe.blockmap",
    "huddle-0.2.0-mac-arm64.dmg",
    "huddle-0.2.0-linux-x86_64.AppImage",
  ]);
});

test("parseLatestRelease tolerates an asset without a name", () => {
  const release = parseLatestRelease(
    releaseBody({ assets: [{ size: 1 }, { name: "huddle-0.2.0-mac-arm64.dmg" }] }),
  );
  assert.deepEqual(release.assets, ["huddle-0.2.0-mac-arm64.dmg"]);
});

test("parseLatestRelease rejects unusable responses", () => {
  for (const bad of [
    "{ not json",
    "",
    null,
    42,
    JSON.stringify({}), // no tag_name
    JSON.stringify({ tag_name: "v0.2.0" }), // no html_url
    JSON.stringify({ tag_name: "", html_url: "https://x" }),
    JSON.stringify({ tag_name: "v0.2.0", html_url: "http://x" }), // not https
    JSON.stringify({ tag_name: "v0.2.0", html_url: "javascript:alert(1)" }),
  ]) {
    assert.equal(parseLatestRelease(bad), null, JSON.stringify(bad));
  }
});

test("hasDesktopAsset matches the platform's installer per OS", () => {
  const release = parseLatestRelease(releaseBody());
  assert.equal(hasDesktopAsset(release, "win32"), true);
  assert.equal(hasDesktopAsset(release, "darwin"), true);
  assert.equal(hasDesktopAsset(release, "linux"), true);
  assert.equal(hasDesktopAsset(release, "freebsd"), false);
  assert.equal(hasDesktopAsset(null, "win32"), false);
});

test("hasDesktopAsset stays false for server-only releases", () => {
  // Tags published before release-desktop.yml existed carry no installers;
  // they must not trigger an update dialog with nothing to download.
  const serverOnly = parseLatestRelease(
    releaseBody({ assets: [{ name: "huddle-server-0.0.1-linux-x86_64.tar.gz" }] }),
  );
  assert.equal(hasDesktopAsset(serverOnly, "darwin"), false);
  assert.equal(hasDesktopAsset(serverOnly, "win32"), false);
  assert.equal(hasDesktopAsset(serverOnly, "linux"), false);
});

test("nextNotifyState notifies once per version, then again for a newer one", () => {
  const first = nextNotifyState(null, { version: "0.2.0" });
  assert.deepEqual(first, { notify: true, lastNotifiedVersion: "0.2.0" });

  // Periodic re-checks of the same version stay silent after "Later".
  assert.deepEqual(nextNotifyState(first.lastNotifiedVersion, { version: "0.2.0" }), {
    notify: false,
    lastNotifiedVersion: "0.2.0",
  });

  const newer = nextNotifyState(first.lastNotifiedVersion, { version: "0.3.0" });
  assert.deepEqual(newer, { notify: true, lastNotifiedVersion: "0.3.0" });

  // No release (parse failure, rate limit) leaves the state untouched.
  assert.deepEqual(nextNotifyState(newer.lastNotifiedVersion, null), {
    notify: false,
    lastNotifiedVersion: "0.3.0",
  });
  assert.deepEqual(nextNotifyState(undefined, null), {
    notify: false,
    lastNotifiedVersion: null,
  });
});
