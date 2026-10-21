import assert from "node:assert/strict";
import { test } from "node:test";
import { parseDeepLink } from "../src/deep-link.mjs";

test("parses a meeting link on a remote host as https", () => {
  assert.deepEqual(parseDeepLink("huddle://meet.example.com/j/abc123", null), {
    serverUrl: "https://meet.example.com",
    path: "/j/abc123",
  });
});

test("parses a link without a path to the root", () => {
  assert.deepEqual(parseDeepLink("huddle://meet.example.com", null), {
    serverUrl: "https://meet.example.com",
    path: "/",
  });
});

test("loopback hosts default to http and keep their port", () => {
  assert.deepEqual(parseDeepLink("huddle://localhost:3000/m/xyz", null), {
    serverUrl: "http://localhost:3000",
    path: "/m/xyz",
  });
  assert.deepEqual(parseDeepLink("huddle://127.0.0.1:8443/", null), {
    serverUrl: "http://127.0.0.1:8443",
    path: "/",
  });
});

test("keeps the query string inside the path", () => {
  assert.deepEqual(parseDeepLink("huddle://meet.example.com:8443/x?y=1", null), {
    serverUrl: "https://meet.example.com:8443",
    path: "/x?y=1",
  });
});

test("reuses the configured scheme when the host matches", () => {
  assert.deepEqual(
    parseDeepLink("huddle://meet.example.com", "https://meet.example.com"),
    { serverUrl: "https://meet.example.com", path: "/" },
  );
  // A LAN server that is genuinely plain http keeps working via reuse...
  assert.deepEqual(
    parseDeepLink("huddle://lanbox.local/j/1", "http://lanbox.local:3000"),
    { serverUrl: "http://lanbox.local", path: "/j/1" },
  );
  // ...while a different host still defaults to https.
  assert.deepEqual(
    parseDeepLink("huddle://other.example/j/1", "http://lanbox.local:3000"),
    { serverUrl: "https://other.example", path: "/j/1" },
  );
});

test("rejects malformed links", () => {
  for (const bad of [
    "",
    "https://meet.example.com/j/abc", // wrong scheme
    "huddle://", // no host
    "huddle:///j/abc", // empty host before path
    "huddle://bad host/j", // space in authority
    "huddle://user:pass@meet.example.com/j", // credentials
    "huddle://user@meet.example.com/j",
    null,
    undefined,
  ]) {
    assert.equal(parseDeepLink(bad, null), null, JSON.stringify(bad));
  }
});

test("tolerates a corrupt configured origin", () => {
  assert.deepEqual(parseDeepLink("huddle://meet.example.com/j/1", "::not a url::"), {
    serverUrl: "https://meet.example.com",
    path: "/j/1",
  });
});
