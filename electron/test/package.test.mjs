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

test("package-lock.json is committed so CI can npm ci", () => {
  const lockPath = path.join(root, "package-lock.json");
  assert.equal(existsSync(lockPath), true);
  const lock = JSON.parse(readFileSync(lockPath, "utf8"));
  const rootPkg = lock.packages[""];
  assert.equal(rootPkg.devDependencies.electron, "45.0.0");
  assert.equal(rootPkg.devDependencies["electron-builder"], "26.15.3");
});

test("electron-builder.yml exists and builds from buildResources", () => {
  const yml = readFileSync(path.join(root, "electron-builder.yml"), "utf8");
  assert.match(yml, /appId:\s*io\.github\.aron98\.huddle/);
  assert.match(yml, /buildResources:\s*build/);
  assert.match(yml, /icon:\s*build\/icon\.png/);
});
