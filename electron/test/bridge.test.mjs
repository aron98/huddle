// Static wiring assertions for the main preload ↔ main-process IPC bridge.
// Issue #107 shipped a connect-preload.js that no BrowserWindow ever
// attached: window.huddleConnect was undefined on the connect page and the
// Connect button silently did nothing. `node --check` cannot catch a missing
// contextBridge exposure, so these source-level assertions pin the wiring.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (rel) => readFileSync(path.join(root, rel), "utf8");

test("the main preload exposes both window bridges (issue #107)", () => {
  const preload = read("src/preload.js");
  assert.match(preload, /exposeInMainWorld\(\s*"huddleDesktop"/, "huddleDesktop bridge");
  assert.match(preload, /exposeInMainWorld\(\s*"huddleConnect"/, "huddleConnect bridge");
});

test("the connect page reads a bridge the main preload actually exposes", () => {
  const connect = read("src/connect.js");
  const preload = read("src/preload.js");
  // Project convention: shell bridges are huddle-prefixed, which excludes
  // plain DOM usage like window.location.
  const wanted = [...connect.matchAll(/window\.(huddle\w+)/g)].map((m) => m[1]);
  assert.ok(wanted.includes("huddleConnect"), "connect.js must read window.huddleConnect");
  for (const name of wanted) {
    assert.match(
      preload,
      new RegExp(`exposeInMainWorld\\(\\s*"${name}"`),
      `preload.js must expose "${name}" for connect.js`,
    );
  }
});

test("every channel the main preload invokes is handled in main.mjs", () => {
  const preload = read("src/preload.js");
  const main = read("src/main.mjs");
  const invoked = [...preload.matchAll(/ipcRenderer\.invoke\(\s*"([^"]+)"/g)].map((m) => m[1]);
  assert.ok(invoked.length > 0, "preload must invoke at least one channel");
  for (const channel of invoked) {
    assert.ok(
      main.includes(`ipcMain.handle("${channel}"`),
      `main.mjs must handle "${channel}"`,
    );
  }
});

test("the main window attaches exactly one main preload", () => {
  const main = read("src/main.mjs");
  const matches = [...main.matchAll(/preload:\s*path\.join\(__dirname,\s*"([^"]+)"\)/g)];
  const attached = matches.map((m) => m[1]);
  assert.ok(attached.includes("preload.js"), "main window must attach preload.js");
  for (const file of attached) {
    assert.notEqual(
      file,
      "connect-preload.js",
      "connect-preload.js is dead code — the connect page runs in the main window",
    );
  }
});
