#!/usr/bin/env node
// Multi-suite e2e runner: the single entry point the CI e2e job calls.
//
// Discovers every `e2e-*.mjs` in this directory and runs them
// sequentially, each as a child process under its own kill timer:
//   - all suites run even when an earlier one fails (the summary lists
//     every broken suite, not just the first),
//   - a hung suite is SIGTERM'd (SIGKILL'd after a grace period) and
//     reported as a failure instead of wedging the whole job — the
//     child runs as its own process group so the server/coturn
//     processes it spawned die with it,
//   - exits non-zero if any suite failed.
//
// Adding a suite means dropping a new `e2e-foo.mjs` next to this file —
// no hand-maintained list, no workflow edit. Suites run in sorted
// (alphabetical) order for deterministic logs.
//
// Env:
//   E2E_SUITE_TIMEOUT_MS  per-suite kill timer (default 10 minutes)
//   ...plus whatever the suites themselves read (CHROME_BIN, ...).

import { spawn } from 'node:child_process';
import { readdirSync } from 'node:fs';
import { join } from 'node:path';

const DIR = import.meta.dirname;
const DEFAULT_TIMEOUT_MS = 10 * 60_000;
const KILL_GRACE_MS = 5_000;

const suites = readdirSync(DIR)
  .filter((f) => /^e2e-.+\.mjs$/.test(f))
  .sort();
// DIAG (ci-debug only): CI's order up to e2e-blur, then e2e-blur twice more.
{
  const upTo = suites.indexOf('e2e-blur.mjs');
  suites.splice(upTo + 1, suites.length, 'e2e-blur.mjs', 'e2e-blur.mjs');
}

if (suites.length === 0) {
  console.error('run-all: no e2e-*.mjs suites found next to run-all.mjs');
  process.exit(1);
}

const timeoutMs = Number(process.env.E2E_SUITE_TIMEOUT_MS ?? DEFAULT_TIMEOUT_MS);

function runSuite(name) {
  return new Promise((resolveSuite) => {
    console.log(`\n=== run-all: ${name} (timeout ${Math.round(timeoutMs / 1000)}s) ===`);
    const startedAt = Date.now();
    // detached: true → the child leads its own process group, so the
    // timeout kill takes down the huddle server / coturn it spawned.
    const child = spawn(process.execPath, [join(DIR, name)], {
      stdio: 'inherit',
      detached: true,
    });

    let timedOut = false;
    const killGroup = (signal) => {
      try {
        process.kill(-child.pid, signal);
      } catch {
        // The group is already gone.
      }
    };
    const timer = setTimeout(() => {
      timedOut = true;
      console.error(`\n=== run-all: ${name} exceeded ${Math.round(timeoutMs / 1000)}s — killing ===`);
      killGroup('SIGTERM');
      // The nuclear option follows if the group ignores SIGTERM.
      setTimeout(() => killGroup('SIGKILL'), KILL_GRACE_MS).unref();
    }, timeoutMs);

    child.on('error', (err) => {
      clearTimeout(timer);
      resolveSuite({ name, ok: false, reason: `spawn failed: ${err.message}`, ms: Date.now() - startedAt });
    });
    child.on('exit', (code, signal) => {
      clearTimeout(timer);
      const ms = Date.now() - startedAt;
      if (timedOut) {
        resolveSuite({ name, ok: false, reason: 'timeout (killed)', ms });
      } else if (code === 0) {
        resolveSuite({ name, ok: true, reason: 'ok', ms });
      } else {
        resolveSuite({ name, ok: false, reason: signal ? `killed by ${signal}` : `exit ${code}`, ms });
      }
    });
  });
}

const fmtSecs = (ms) => `${Math.round(ms / 1000)}s`;

const results = [];
for (const suite of suites) {
  results.push(await runSuite(suite));
}

// Summary table: every suite, pass or fail, with its result.
const nameCol = Math.max(...suites.map((s) => s.length));
console.log(`\n=== run-all: summary ===`);
for (const r of results) {
  console.log(
    `${r.ok ? 'PASS' : 'FAIL'}  ${r.name.padEnd(nameCol)}  ${r.reason}  (${fmtSecs(r.ms)})`,
  );
}
const failed = results.filter((r) => !r.ok);
if (failed.length > 0) {
  console.error(`\nrun-all: ${failed.length}/${results.length} suite(s) failed: ${failed.map((r) => r.name).join(', ')}`);
  process.exit(1);
}
console.log(`run-all: all ${results.length} suite(s) passed`);
