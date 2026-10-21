#!/usr/bin/env node
// Generates build/icon.png (512x512 colored) and build/trayTemplate.png
// (32x32 monochrome-with-alpha) with zero npm dependencies: a minimal
// hand-rolled PNG encoder (IHDR + zlib IDAT + CRC32) on top of per-pixel
// coverage computed by supersampling.
//
// Geometry: a teal rounded square with a white lowercase 'h' built from
// three rectangles (stem, crossbar, right leg). The tray icon is the same
// 'h' silhouette as black-with-alpha so macOS treats it as a template
// image (icon file name carries the "Template" suffix).

import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import zlib from "node:zlib";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const OUT_DIR = path.join(__dirname, "..", "build");

// --- PNG encoding -----------------------------------------------------------

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c;
  }
  return table;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const typeBuf = Buffer.from(type, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])));
  return Buffer.concat([len, typeBuf, data, crc]);
}

// rgba: Buffer of width*height*4 bytes
function encodePng(width, height, rgba) {
  const signature = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // color type: RGBA
  ihdr[10] = 0; // compression: deflate
  ihdr[11] = 0; // filter: adaptive
  ihdr[12] = 0; // interlace: none
  // Raw scanlines: each row prefixed with filter byte 0 (None).
  const stride = width * 4;
  const raw = Buffer.alloc(height * (stride + 1));
  for (let y = 0; y < height; y++) {
    const row = y * (stride + 1);
    raw[row] = 0;
    rgba.copy(raw, row + 1, y * stride, (y + 1) * stride);
  }
  const idat = zlib.deflateSync(raw, { level: 9 });
  return Buffer.concat([
    signature,
    chunk("IHDR", ihdr),
    chunk("IDAT", idat),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// --- geometry ----------------------------------------------------------------
// All shapes are described in unit coordinates [0,1]x[0,1]; coverage is
// computed by supersampling each output pixel and averaging hit counts.

const CORNER_RADIUS = 0.1875; // ~96/512
const GLYPH = {
  // lowercase 'h' from three overlapping rects (x0, y0, x1, y1)
  stem: { x0: 0.28, y0: 0.16, x1: 0.46, y1: 0.84 },
  bar: { x0: 0.28, y0: 0.42, x1: 0.72, y1: 0.58 },
  leg: { x0: 0.54, y0: 0.42, x1: 0.72, y1: 0.84 },
};

function inRect(r, x, y) {
  return x >= r.x0 && x < r.x1 && y >= r.y0 && y < r.y1;
}

function inGlyph(x, y) {
  return inRect(GLYPH.stem, x, y) || inRect(GLYPH.bar, x, y) || inRect(GLYPH.leg, x, y);
}

// Rounded-rect coverage with hard edge; AA comes from supersampling.
function inRoundedRect(x, y, radius) {
  const w = 1 - 2 * radius;
  const cx = Math.min(Math.max(x - radius, 0), w);
  const cy = Math.min(Math.max(y - radius, 0), w);
  const dx = x - radius - cx;
  const dy = y - radius - cy;
  return dx * dx + dy * dy <= radius * radius;
}

// Renders `coverage(x, y) -> boolean` at `size` px with `ss`x supersampling;
// returns a Float alpha map (0..1 per pixel).
function renderCoverage(size, ss, testFn) {
  const big = size * ss;
  const hits = new Float64Array(size * size);
  for (let by = 0; by < big; by++) {
    const y = (by + 0.5) / big;
    const row = ((by / ss) | 0) * size;
    for (let bx = 0; bx < big; bx++) {
      const x = (bx + 0.5) / big;
      if (testFn(x, y)) hits[row + ((bx / ss) | 0)] += 1;
    }
  }
  const area = ss * ss;
  for (let i = 0; i < hits.length; i++) hits[i] /= area;
  return hits;
}

function alphaToRgba(size, alpha, rgb) {
  const buf = Buffer.alloc(size * size * 4);
  for (let i = 0; i < size * size; i++) {
    const o = i * 4;
    buf[o] = rgb[0];
    buf[o + 1] = rgb[1];
    buf[o + 2] = rgb[2];
    buf[o + 3] = Math.round(alpha[i] * 255);
  }
  return buf;
}

// --- output ------------------------------------------------------------------

const ICON_SIZE = 512;
const TRAY_SIZE = 32;
const SUPERSAMPLE = 4; // 2048x2048 samples for the big icon

const TEAL = [13, 148, 136]; // #0d9488
const BLACK = [0, 0, 0];

mkdirSync(OUT_DIR, { recursive: true });

const iconAlpha = renderCoverage(ICON_SIZE, SUPERSAMPLE, (x, y) =>
  inRoundedRect(x, y, CORNER_RADIUS) && !inGlyph(x, y),
);
const iconPng = encodePng(ICON_SIZE, ICON_SIZE, alphaToRgba(ICON_SIZE, iconAlpha, TEAL));
writeFileSync(path.join(OUT_DIR, "icon.png"), iconPng);

const trayAlpha = renderCoverage(TRAY_SIZE, SUPERSAMPLE, (x, y) => inGlyph(x, y));
const trayPng = encodePng(TRAY_SIZE, TRAY_SIZE, alphaToRgba(TRAY_SIZE, trayAlpha, BLACK));
writeFileSync(path.join(OUT_DIR, "trayTemplate.png"), trayPng);

console.log(`wrote build/icon.png (${ICON_SIZE}x${ICON_SIZE}, ${iconPng.length} bytes)`);
console.log(`wrote build/trayTemplate.png (${TRAY_SIZE}x${TRAY_SIZE}, ${trayPng.length} bytes)`);
