#!/usr/bin/env node
// Syntax-checks every shell .js/.mjs file with `node --check` (parse only —
// no Electron needed, so it runs in plain CI). Preloads are CommonJS-style
// plain scripts (package.json has no "type"), main-process modules are ESM;
// node --check handles both from the file extension.

import { readdirSync, statSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const SCAN_DIRS = ["src", "scripts", "test"];

function collectJsFiles(dir, out = []) {
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) {
      collectJsFiles(full, out);
    } else if (/\.(js|mjs)$/.test(entry)) {
      out.push(full);
    }
  }
  return out;
}

const files = SCAN_DIRS.flatMap((dir) => {
  try {
    return collectJsFiles(path.join(root, dir));
  } catch {
    return []; // directory not present — fine in stripped checkouts
  }
}).sort();

if (files.length === 0) {
  console.error("check-syntax: no .js/.mjs files found under src/, scripts/, test/");
  process.exit(1);
}

const failures = [];
for (const file of files) {
  const result = spawnSync(process.execPath, ["--check", file], { encoding: "utf8" });
  if (result.status !== 0) {
    failures.push({ file, stderr: result.stderr });
  }
}

for (const { file, stderr } of failures) {
  console.error(`FAIL ${path.relative(root, file)}\n${stderr}`);
}
if (failures.length > 0) {
  process.exit(1);
}
console.log(`check-syntax: ${files.length} files OK`);
