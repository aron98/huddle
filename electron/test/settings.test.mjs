import assert from "node:assert/strict";
import { test } from "node:test";
import {
  loadSettings,
  normalizeServerUrl,
  saveSettings,
  settingsPath,
} from "../src/settings.mjs";

test("settingsPath joins the userData dir", () => {
  assert.equal(settingsPath("/home/me/.huddle"), "/home/me/.huddle/settings.json");
});

test("normalizeServerUrl accepts bare origins with the right default scheme", () => {
  assert.deepEqual(normalizeServerUrl("localhost:3000"), {
    ok: true,
    url: "http://localhost:3000",
  });
  assert.deepEqual(normalizeServerUrl("127.0.0.1:8443"), {
    ok: true,
    url: "http://127.0.0.1:8443",
  });
  assert.deepEqual(normalizeServerUrl("meet.example.com"), {
    ok: true,
    url: "https://meet.example.com",
  });
  assert.deepEqual(normalizeServerUrl("https://meet.example.com/"), {
    ok: true,
    url: "https://meet.example.com",
  });
  assert.deepEqual(normalizeServerUrl("  http://localhost:3000  "), {
    ok: true,
    url: "http://localhost:3000",
  });
});

test("normalizeServerUrl keeps ports and lowercases scheme/host", () => {
  assert.deepEqual(normalizeServerUrl("HTTPS://Meet.Example.Com:8443"), {
    ok: true,
    url: "https://meet.example.com:8443",
  });
});

test("normalizeServerUrl rejects non-root paths, queries and hashes", () => {
  for (const bad of [
    "https://meet.example.com/j/abc",
    "https://meet.example.com/?token=1",
    "https://meet.example.com/#room",
  ]) {
    assert.equal(normalizeServerUrl(bad).ok, false, bad);
  }
});

test("normalizeServerUrl rejects junk input", () => {
  for (const bad of [
    "",
    "   ",
    null,
    undefined,
    42,
    "huddle://meet.example.com", // deep-link scheme is not a server address
    "ftp://meet.example.com",
    "https://user:pass@meet.example.com",
    "https://user@meet.example.com",
    "http://",
    "not a url",
  ]) {
    assert.equal(normalizeServerUrl(bad).ok, false, JSON.stringify(bad));
  }
});

test("loadSettings reads a saved serverUrl", () => {
  const fs = memFs({ "settings.json": JSON.stringify({ serverUrl: "https://x.example" }) });
  assert.deepEqual(loadSettings(fs, "settings.json"), { serverUrl: "https://x.example" });
});

test("loadSettings treats missing and corrupt files as unset", () => {
  const missing = memFs({});
  assert.deepEqual(loadSettings(missing, "settings.json"), { serverUrl: null });

  const corrupt = memFs({ "settings.json": "{ not json" });
  assert.deepEqual(loadSettings(corrupt, "settings.json"), { serverUrl: null });

  const wrongType = memFs({ "settings.json": JSON.stringify({ serverUrl: 42 }) });
  assert.deepEqual(loadSettings(wrongType, "settings.json"), { serverUrl: null });
});

test("saveSettings round-trips through loadSettings and creates the dir", () => {
  const fs = memFs({});
  saveSettings(fs, "/userData/settings.json", { serverUrl: "http://localhost:3000" });
  assert.deepEqual(fs.mkdirCalls, ["/userData"]);
  assert.deepEqual(loadSettings(fs, "/userData/settings.json"), {
    serverUrl: "http://localhost:3000",
  });
});

function memFs(files) {
  return {
    files,
    mkdirCalls: [],
    readFileSync(file) {
      if (!(file in files)) {
        const err = new Error("ENOENT");
        err.code = "ENOENT";
        throw err;
      }
      return files[file];
    },
    writeFileSync(file, data) {
      files[file] = data;
    },
    mkdirSync(dir) {
      this.mkdirCalls.push(dir);
    },
  };
}
