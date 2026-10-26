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

test("macOS builds are ad-hoc signed, not unsigned (issue #86)", () => {
  const yml = readFileSync(path.join(root, "electron-builder.yml"), "utf8");
  // electron-builder 26.15.3: identity "-" → ad-hoc signature; identity: null
  // would SKIP signing entirely and arm64 Gatekeeper reports the app as
  // "broken and can't be opened" (issue #86). Must stay the string "-".
  assert.match(yml, /identity:\s*"-"(\s|$)/m);
});
