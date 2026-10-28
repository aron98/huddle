import assert from "node:assert/strict";
import { test } from "node:test";
import { describeLoadError } from "../src/load-errors.mjs";

// Electron loadURL rejections carry the Chromium net error code in the
// Error message, e.g. `Error: ERR_CONNECTION_REFUSED: -102`.
test("maps common net errors to friendly, code-tagged messages", () => {
  const refused = describeLoadError(new Error("ERR_CONNECTION_REFUSED: -102"));
  assert.match(refused, /Could not reach the server/);
  assert.match(refused, /refused the connection/);
  assert.match(refused, /\(net error ERR_CONNECTION_REFUSED\)$/);

  assert.match(describeLoadError(new Error("ERR_NAME_NOT_RESOLVED: -105")), /Could not resolve/);
  assert.match(describeLoadError(new Error("ERR_CONNECTION_TIMED_OUT: -118")), /timed out/);
});

test("TLS errors hint at the certificate or the http scheme", () => {
  const selfSigned = describeLoadError(new Error("ERR_CERT_AUTHORITY_INVALID: -202"));
  assert.match(selfSigned, /certificate/);
  assert.match(selfSigned, /net error ERR_CERT_AUTHORITY_INVALID/);

  assert.match(describeLoadError(new Error("ERR_CERT_DATE_INVALID: -201")), /expired/);
  assert.match(describeLoadError(new Error("ERR_SSL_PROTOCOL_ERROR: -107")), /http:\/\//);
});

test("recognizes the code on err.code as well as in the message", () => {
  const err = new Error("load failed");
  err.code = "ERR_TIMED_OUT";
  assert.match(describeLoadError(err), /\(net error ERR_TIMED_OUT\)$/);
});

test("unknown net errors keep the raw code for diagnosability", () => {
  const out = describeLoadError(new Error("ERR_SOMETHING_EXOTIC: -609"));
  assert.match(out, /Could not load the server page/);
  assert.match(out, /\(net error ERR_SOMETHING_EXOTIC\)\./);
});

test("non-net errors fall back to a generic message with the raw detail", () => {
  const out = describeLoadError(new Error("Object has been destroyed"));
  assert.match(out, /Could not load the server page/);
  assert.match(out, /Object has been destroyed/);
  assert.ok(!out.includes("net error"), "no fabricated net error code");
});

test("never throws on junk input", () => {
  for (const junk of [null, undefined, 42, "", "plain string", { code: 7 }, new Error("")]) {
    assert.ok(typeof describeLoadError(junk) === "string", String(junk));
  }
  assert.match(describeLoadError(""), /unknown error/);
});
