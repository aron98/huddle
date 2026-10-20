#!/usr/bin/env node
// Per-meeting passcode end-to-end test: refusal, wrong-passcode
// refusal, success, and removal (issue #13).
//
// Boots a real huddle server, creates a passcode-protected meeting via
// the REST API, and drives the real prejoin page in headless Chrome:
// a guest without the passcode is refused with an inline error on the
// passcode field (and never leaves the prejoin page), a wrong passcode
// is refused identically (no oracle distinguishing wrong from
// missing), the right passcode joins the call, and after the organizer
// removes the passcode via PATCH a fresh guest joins with no passcode
// field at all. A no-passcode meeting never shows the field.

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

const PORT = 3136;
const BASE = `http://127.0.0.1:${PORT}`;
const CHROME = process.env.CHROME_BIN ?? 'google-chrome';
const ROOT = resolve(import.meta.dirname, '../..');
const SERVER = join(ROOT, 'target/debug/huddle');
const PASSCODE = 'e2e-huddle-pass';

// Fail fast, but unwind through `finally` so the server/browser clean up.
class FailSignal extends Error {}

const fail = (msg) => {
  console.error(`FAIL: ${msg}`);
  process.exitCode = 1;
  throw new FailSignal(msg);
};

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(what, fn, timeoutMs = 60_000, everyMs = 500) {
  const deadline = Date.now() + timeoutMs;
  let last;
  for (;;) {
    last = await fn();
    if (last) return last;
    if (Date.now() > deadline) fail(`timed out waiting for ${what}: ${JSON.stringify(last)}`);
    await delay(everyMs);
  }
}

async function bootServer() {
  const dataDir = mkdtempSync(join(tmpdir(), 'huddle-e2e-'));
  const server = spawn(SERVER, [], {
    cwd: ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      RUST_LOG: process.env.RUST_LOG ?? 'huddle=debug',
      HUDDLE_BIND: `127.0.0.1:${PORT}`,
      HUDDLE_PUBLIC_IP: '127.0.0.1',
      HUDDLE_MEDIA_PORT: '0',
      HUDDLE_DATA_DIR: dataDir,
      HUDDLE_WEB_DIR: join(ROOT, 'crates/web/dist'),
    },
  });
  let logs = '';
  server.stdout.on('data', (d) => (logs += d));
  server.stderr.on('data', (d) => (logs += d));

  await waitFor('server health', async () => {
    try {
      const res = await fetch(`${BASE}/api/health`);
      return res.ok;
    } catch {
      return false;
    }
  }, 30_000);

  return {
    server,
    logs: () => logs,
    cleanup: () => {
      server.kill('SIGTERM');
      rmSync(dataDir, { recursive: true, force: true });
    },
  };
}

async function createMeeting(passcode) {
  const rfc3339 = (date) => date.toISOString().replace(/\.\d+Z$/, 'Z');
  const body = {
    title: passcode ? 'e2e passcode' : 'e2e open',
    starts_at: rfc3339(new Date()),
    ends_at: rfc3339(new Date(Date.now() + 60 * 60 * 1000)),
  };
  if (passcode) body.passcode = passcode;
  const res = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) fail(`meeting creation failed: ${res.status} ${await res.text()}`);
  const text = await res.text();
  const meeting = JSON.parse(text).meeting;
  return { code: meeting.code, text, meeting };
}

async function patchPasscode(code, passcode) {
  // Tri-state wire format: null removes, a string sets, absent keeps.
  const res = await fetch(`${BASE}/api/meetings/${code}`, {
    method: 'PATCH',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(passcode === null ? { passcode: null } : { passcode }),
  });
  if (!res.ok) fail(`passcode patch failed: ${res.status} ${await res.text()}`);
  return res.json();
}

// Open the prejoin page and get as far as a clickable join button.
async function openPrejoin(page, code, name) {
  await page.goto(`${BASE}/m/${code}?test=1`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-testid="name-input"]');
  await page.type('[data-testid="name-input"]', name);
  await page.waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: media never became ready`));
}

async function clickJoin(page) {
  await page.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
}

async function passcodeError(page) {
  return page.evaluate(() => {
    const errors = [...document.querySelectorAll('.field-error')].map((e) => e.textContent);
    const passcodeField = !!document.querySelector('[data-testid="passcode-input"]');
    return { errors, passcodeField };
  });
}

// Refusal = still on the prejoin page with a passcode inline error.
async function expectRefusal(page, who) {
  const state = await waitFor(`${who} refused with a passcode error`, async () => {
    // The join round trip unmounts prejoin briefly (Joining phase);
    // wait for the form to be back before probing.
    const nameThere = await page.evaluate(
      () => !!document.querySelector('[data-testid="name-input"]'),
    );
    if (!nameThere) return null;
    const snap = await passcodeError(page);
    return snap.passcodeField && snap.errors.some((e) => /passcode/i.test(e)) ? snap : null;
  }, 30_000).catch(async () => {
    const snap = await passcodeError(page).catch(() => null);
    fail(`${who}: no passcode refusal appeared: ${JSON.stringify(snap)}`);
  });
  const stillPrejoin = await page.evaluate(() => !!document.querySelector('[data-testid="name-input"]'));
  if (!stillPrejoin) fail(`${who}: left the prejoin page despite the passcode`);
  console.log(`${who} refused: ${JSON.stringify(state)}`);
}

// Success = the roster shows the joiner and the prejoin form is gone.
async function expectInCall(page, who) {
  // The roster entry only exists in the call view; the prejoin page is
  // unmounted during the transient Joining phase, so waiting on the
  // roster (not on name-input disappearing) proves the join took.
  await page.waitForSelector(`[data-testid="roster-${who}"]`, { timeout: 30_000 })
    .catch(async () => {
      const snap = await passcodeError(page).catch(() => null);
      fail(`${who}: never made it into the call: ${JSON.stringify(snap)}`);
    });
  const stillPrejoin = await page.evaluate(() => !!document.querySelector('[data-testid="name-input"]'));
  if (stillPrejoin) fail(`${who}: still on the prejoin page despite joining`);
  console.log(`${who} is in the call`);
}

async function main() {
  console.log(`booting server (${SERVER})`);
  const srv = await bootServer();
  let browser;
  try {
    const { code, text, meeting } = await createMeeting(PASSCODE);
    if (meeting.has_passcode !== true) fail(`created meeting has no has_passcode flag: ${text}`);
    if (text.includes(PASSCODE)) fail('the create response leaked the passcode value');
    console.log(`meeting ${code} created with a passcode (value absent from the API response)`);

    browser = await puppeteer.launch({
      executablePath: CHROME,
      headless: true,
      args: [
        '--no-sandbox',
        '--disable-gpu',
        '--disable-dev-shm-usage',
        '--use-fake-device-for-media-stream',
        '--use-fake-ui-for-media-stream',
        '--autoplay-policy=no-user-gesture-required',
      ],
    });

    const newClient = async (who) => {
      const page = await browser.newPage();
      page.on('console', (m) => {
        if (m.type() !== 'warning') console.log(`[${who} console:${m.type()}]`, m.text());
      });
      page.on('pageerror', (e) => console.log(`[${who} pageerror]`, e.message));
      return page;
    };

    const guest = await newClient('guest');
    console.log('guest opens the prejoin page — the passcode field shows proactively');
    await openPrejoin(guest, code, 'guest');
    await guest
      .waitForSelector('[data-testid="passcode-input"]', { timeout: 30_000 })
      .catch(async () => fail('the passcode field did not show on a protected meeting'));

    console.log('join without a passcode is refused with an inline error');
    await clickJoin(guest);
    await expectRefusal(guest, 'guest');

    console.log('a wrong passcode is refused identically');
    await guest.type('[data-testid="passcode-input"]', 'not-the-passcode');
    await clickJoin(guest);
    await expectRefusal(guest, 'guest');

    console.log('the right passcode joins the call');
    // The field keeps what was typed (the wrong attempt); clear it first
    // so the keystrokes below land in an empty input.
    await guest.evaluate(() => {
      const el = document.querySelector('[data-testid="passcode-input"]');
      el.value = '';
      el.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await guest.type('[data-testid="passcode-input"]', PASSCODE);
    await clickJoin(guest);
    await expectInCall(guest, 'guest');
    await guest.close();

    console.log('removing the passcode via PATCH');
    const patched = await patchPasscode(code, null);
    if (patched.meeting.has_passcode !== false) {
      fail(`passcode removal did not clear the flag: ${JSON.stringify(patched)}`);
    }

    console.log('a fresh guest joins with no passcode field at all');
    const later = await newClient('late-guest');
    await openPrejoin(later, code, 'late-guest');
    const fieldShown = await later.evaluate(
      () => !!document.querySelector('[data-testid="passcode-input"]'),
    );
    if (fieldShown) fail('the passcode field still shows after removal');
    await clickJoin(later);
    await expectInCall(later, 'late-guest');
    await later.close();

    console.log('a no-passcode meeting never shows the field');
    const open = await createMeeting(undefined);
    const plain = await newClient('plain-guest');
    await openPrejoin(plain, open.code, 'plain-guest');
    const plainField = await plain.evaluate(
      () => !!document.querySelector('[data-testid="passcode-input"]'),
    );
    if (plainField) fail('the passcode field shows on an open meeting');
    await clickJoin(plain);
    await expectInCall(plain, 'plain-guest');
    await plain.close();

    console.log('PASS: passcode refusal, wrong-passcode refusal, success, and removal all behave');
  } finally {
    if (browser) await browser.close().catch(() => {});
    srv.cleanup();
    if (process.exitCode) console.log('--- server logs ---\n' + srv.logs().slice(-4000));
  }
}

main().catch((e) => {
  if (!(e instanceof FailSignal)) {
    console.error(e);
    process.exitCode = 1;
  }
});
