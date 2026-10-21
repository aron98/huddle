import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const buildDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "build");

const PNG_SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

function readPng(file) {
  const png = readFileSync(path.join(buildDir, file));
  assert.ok(png.subarray(0, 8).equals(PNG_SIGNATURE), `${file}: bad PNG signature`);
  assert.equal(png.readUInt32BE(8), 13, `${file}: IHDR must be 13 bytes`);
  assert.equal(png.subarray(12, 16).toString("ascii"), "IHDR", `${file}: IHDR must come first`);
  return {
    width: png.readUInt32BE(16),
    height: png.readUInt32BE(20),
    bitDepth: png[24],
    colorType: png[25],
  };
}

test("build/icon.png is a 512x512 RGBA PNG", () => {
  const { width, height, bitDepth, colorType } = readPng("icon.png");
  assert.equal(width, 512);
  assert.equal(height, 512);
  assert.equal(bitDepth, 8);
  assert.equal(colorType, 6); // RGBA
});

test("build/trayTemplate.png is a 32x32 RGBA PNG", () => {
  const { width, height, bitDepth, colorType } = readPng("trayTemplate.png");
  assert.equal(width, 32);
  assert.equal(height, 32);
  assert.equal(bitDepth, 8);
  assert.equal(colorType, 6); // RGBA — black pixels + alpha (template style)
});
