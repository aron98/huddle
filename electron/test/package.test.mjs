import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

test("package.json parses and pins the exact expected toolchain", () => {
  const pkg = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8"));
  assert.equal(pkg.main, "src/main.mjs");
  assert.equal(existsSync(path.join(root, pkg.main)), true, "main entry must exist");
  assert.equal(pkg.devDependencies.electron, "45.0.0", "electron must be exactly pinned");
  assert.equal(
    pkg.devDependencies["electron-builder"],
    "26.15.3",
    "electron-builder must be exactly pinned",
  );
});

test("electron-updater is an exactly-pinned production dependency (issue #98)", () => {
  const pkg = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8"));
  // Runtime dep, not dev: electron-builder bundles production node_modules
  // into the app, and setFeedURL is configured in code — the pin must stay
  // exact for reproducible release builds.
  assert.equal(pkg.dependencies["electron-updater"], "6.8.11");
  assert.ok(!pkg.devDependencies["electron-updater"], "must not be a devDependency");
});

test("package-lock.json matches the pinned dependencies", () => {
  const lockPath = path.join(root, "package-lock.json");
  assert.equal(existsSync(lockPath), true, "lock file must be committed for CI npm ci");
  const lock = JSON.parse(readFileSync(lockPath, "utf8"));
  const rootPkg = lock.packages[""];
  assert.equal(rootPkg.devDependencies.electron, "45.0.0");
  assert.equal(rootPkg.devDependencies["electron-builder"], "26.15.3");
  assert.equal(rootPkg.dependencies["electron-updater"], "6.8.11");
  assert.equal(lock.packages["node_modules/electron-updater"]?.version, "6.8.11");
});

test("electron-builder.yml exists and builds from buildResources", () => {
  const yml = readFileSync(path.join(root, "electron-builder.yml"), "utf8");
  assert.match(yml, /appId:\s*io\.github\.aron98\.huddle/);
  assert.match(yml, /buildResources:\s*build/);
  assert.match(yml, /icon:\s*build\/icon\.png/);
});

test("macOS builds are Developer ID signed and notarized (issue #103)", () => {
  const yml = readFileSync(path.join(root, "electron-builder.yml"), "utf8");
  // `identity` is intentionally OMITTED: electron-builder auto-detects the
  // sole "Developer ID Application" cert in the keychain the release
  // workflow creates. "-" (ad-hoc, the old issue #86 workaround) and null
  // (skip signing) both make notarization impossible, and an explicit
  // "Developer ID Application: <CN>" value is rejected by findIdentity.
  assert.doesNotMatch(yml, /^\s*identity:/m, "identity must stay omitted (auto-detected)");
  // Notarization requires the hardened runtime; the workflow's
  // spctl/stapler verification requires the app to be notarized.
  // (Indented on purpose under `mac:` — the \s* allows that but still
  // excludes the prose comments, which have `#` after the whitespace.)
  assert.match(yml, /^\s*hardenedRuntime:\s*true$/m);
  assert.match(yml, /^\s*notarize:\s*true$/m);
});

test("release-desktop.yml publishes the electron-updater feed files (issue #98)", () => {
  const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
  const workflow = readFileSync(
    path.join(repoRoot, ".github", "workflows", "release-desktop.yml"),
    "utf8",
  );
  // electron-updater reads latest.yml (NSIS) / latest-linux.yml (AppImage)
  // to discover the current version + artifact URL, and uses the .blockmap
  // for differential downloads. Without these in the GitHub release the
  // packaged app's update check silently finds nothing.
  for (const needed of [
    "electron/dist/latest.yml",
    "electron/dist/latest-linux.yml",
    ".exe.blockmap",
    ".AppImage.blockmap",
  ]) {
    assert.ok(workflow.includes(needed), `release must attach ${needed}`);
  }
});

test("release-desktop.yml notarizes macOS via the macos-signing environment (issue #103)", () => {
  const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
  const workflow = readFileSync(
    path.join(repoRoot, ".github", "workflows", "release-desktop.yml"),
    "utf8",
  );
  // The signing secrets are scoped to a dedicated GitHub environment that
  // only the macOS job opts into — the Windows/Linux matrix job must
  // never see them (that is why macOS is a separate job, not a matrix
  // leg: `environment:` is job-level).
  assert.match(workflow, /environment:\s*macos-signing/);
  assert.match(workflow, /secrets\.MAC_CERTS_P12_BASE64/);
  assert.match(workflow, /secrets\.MAC_CERTS_PASSWORD/);
  assert.match(workflow, /secrets\.APPLE_ID/);
  assert.match(workflow, /secrets\.APPLE_APP_SPECIFIC_PASSWORD/);
  // The job imports the p12 into a temporary keychain (codesign must be
  // able to use the key non-interactively) and derives APPLE_TEAM_ID —
  // required by notarization's APPLE_ID flow — from the imported cert.
  assert.match(workflow, /security import .*-T \/usr\/bin\/codesign/);
  assert.match(workflow, /set-key-partition-list/);
  assert.match(workflow, /APPLE_TEAM_ID=/);
  // Post-build verification must prove Developer ID signing,
  // notarization and the stapled ticket before anything is published —
  // electron-builder 26.15.3 skips notarization with only a warning if
  // credentials are missing, so these checks are the enforcement.
  assert.match(workflow, /codesign --verify --deep --strict/);
  assert.match(workflow, /spctl --assess --type execute/);
  assert.match(workflow, /xcrun stapler validate/);
});
