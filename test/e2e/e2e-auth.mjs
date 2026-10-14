#!/usr/bin/env node
// Magic-link auth + scheduling dashboard e2e (phase 5).
//
// Boots a real huddle server and asserts, against a real browser:
//   - anonymous meeting creation via the API still works,
//   - the dashboard asks anonymous visitors to sign in,
//   - a magic link is issued (logged when SMTP is unconfigured), lands a
//     session cookie, and the browser renders the organizer dashboard,
//   - meetings can be created from the dashboard UI, exported as ICS,
//     and cancelled,
//   - ownership is enforced (anonymous and non-owner PATCH → 403).

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

const PORT = 3132;
const BASE = `http://127.0.0.1:${PORT}`;
const CHROME = process.env.CHROME_BIN ?? 'google-chrome';
const ROOT = resolve(import.meta.dirname, '../..');
const SERVER = join(ROOT, 'target/debug/huddle');

// Fail fast, but unwind through `finally` so the server/browser clean up.
class FailSignal extends Error {}

const fail = (msg) => {
  console.error(`FAIL: ${msg}`);
  process.exitCode = 1;
  throw new FailSignal(msg);
};

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(what, fn, timeoutMs = 20_000, everyMs = 500) {
  const deadline = Date.now() + timeoutMs;
  let last;
  for (;;) {
    last = await fn();
    if (last) return last;
    if (Date.now() > deadline) fail(`timed out waiting for ${what}: ${JSON.stringify(last)}`);
    await delay(everyMs);
  }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

async function bootServer() {
  const dataDir = mkdtempSync(join(tmpdir(), 'huddle-auth-e2e-'));
  const server = spawn(SERVER, [], {
    cwd: ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      RUST_LOG: process.env.RUST_LOG ?? 'huddle=info',
      HUDDLE_BIND: `127.0.0.1:${PORT}`,
      // Magic links are built from the configured base URL — point it at
      // this instance so the logged link is followable.
      HUDDLE_BASE_URL: BASE,
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

const rfc3339 = (date) => date.toISOString().replace(/\.\d+Z$/, 'Z');

// ---------------------------------------------------------------------------
// Magic-link helpers
// ---------------------------------------------------------------------------

async function requestMagicLink(email) {
  const res = await fetch(`${BASE}/api/auth/request-link`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ email }),
  });
  if (!res.ok) fail(`request-link for ${email} returned ${res.status}`);
}

async function magicLinkFrom(logs) {
  // Take the LAST logged link: earlier magic links stay in the log, so a
  // fresh match on the first occurrence would replay a consumed token.
  return waitFor('magic link in server logs', async () => {
    const matches = logs().match(/https?:\/\/\S+\/api\/auth\/verify\?token=\S+/g);
    return matches ? matches.at(-1) : null;
  });
}

async function followLink(link) {
  const res = await fetch(link, { redirect: 'manual' });
  const cookie = res.headers.get('set-cookie');
  if (res.status !== 303 || !cookie || !cookie.startsWith('huddle_session=')) {
    fail(`verify link did not issue a session: status=${res.status} cookie=${cookie}`);
  }
  return cookie;
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

const browser = await puppeteer.launch({
  executablePath: CHROME,
  headless: true,
  args: ['--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage'],
});
const server = await bootServer();

try {
  // Anonymous creation still works.
  const anonymous = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: 'Anonymous standup',
      starts_at: rfc3339(new Date(Date.now() + 10 * 60_000)),
      ends_at: rfc3339(new Date(Date.now() + 70 * 60_000)),
    }),
  });
  if (anonymous.status !== 201) fail(`anonymous create returned ${anonymous.status}`);
  console.log('anonymous API create still works');

  // The dashboard asks anonymous visitors to sign in.
  const page = await browser.newPage();
  page.on('pageerror', (error) => console.log(`[pageerror] ${error.message}`));
  await page.goto(`${BASE}/meetings`);
  await page.waitForSelector('[data-testid="signed-out"]', { timeout: 10_000 });
  console.log('anonymous dashboard shows signed-out');

  // Magic link for the test organizer (SMTP unconfigured → link is logged).
  await requestMagicLink('test@example.com');
  const link = await magicLinkFrom(server.logs);
  console.log(`magic link logged: ${link}`);

  // The browser follows the link: verify consumes the single-use token,
  // sets the session cookie, and redirects to /meetings where the
  // dashboard renders. (The link cannot be fetched twice — the second
  // use would be rejected — so the browser is the one to consume it.)
  await page.goto(link, { waitUntil: 'networkidle0' });
  await page.waitForSelector('[data-testid="create-btn"]', { timeout: 15_000 });
  console.log('magic link signed the browser in; dashboard renders');

  // Create a meeting from the UI. datetime-local inputs do not take typed
  // text reliably — set the value programmatically and dispatch input.
  await page.type('[data-testid="create-title"]', 'Quarterly review');
  await page.$eval('[data-testid="create-starts"]', (el, v) => {
    el.value = v;
    el.dispatchEvent(new Event('input', { bubbles: true }));
  }, '2026-10-15T10:00');
  await page.$eval('[data-testid="create-ends"]', (el, v) => {
    el.value = v;
    el.dispatchEvent(new Event('input', { bubbles: true }));
  }, '2026-10-15T11:00');
  await page.click('[data-testid="create-btn"]');

  const row = await page.waitForSelector('[data-testid^="meeting-row-"]', { timeout: 15_000 });
  const rowTestid = await row.evaluate((el) => el.getAttribute('data-testid'));
  const code = rowTestid.replace('meeting-row-', '');
  console.log(`created meeting ${code} from the dashboard`);

  // ICS export: correct type and content.
  const ics = await fetch(`${BASE}/api/meetings/${code}/ics`);
  const icsBody = await ics.text();
  if (ics.status !== 200) fail(`ICS download returned ${ics.status}`);
  const icsType = ics.headers.get('content-type') ?? '';
  if (!icsType.includes('text/calendar')) fail(`ICS content-type was ${icsType}`);
  if (!icsBody.includes('BEGIN:VCALENDAR')) fail('ICS body is missing BEGIN:VCALENDAR');
  if (!icsBody.includes('SUMMARY:Quarterly review')) fail('ICS body is missing the title');
  console.log('ICS export works');

  // Ownership: anonymous PATCH forbidden, owner PATCH works (via the
  // browser's session cookie, using same-origin fetch), non-owner forbidden.
  const patch = (title) => JSON.stringify({ title });
  const anonymousPatch = await fetch(`${BASE}/api/meetings/${code}`, {
    method: 'PATCH',
    headers: { 'content-type': 'application/json' },
    body: patch('hijack'),
  });
  if (anonymousPatch.status !== 403) fail(`anonymous PATCH returned ${anonymousPatch.status}`);

  const ownerPatchStatus = await page.evaluate(async (c) => {
    const res = await fetch(`/api/meetings/${c}`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ title: 'Quarterly review v2' }),
    });
    return res.status;
  }, code);
  if (ownerPatchStatus !== 200) fail(`owner PATCH returned ${ownerPatchStatus}`);

  await requestMagicLink('eve@example.com');
  const eveLink = await magicLinkFrom(server.logs);
  const eveCookie = await followLink(eveLink);
  const evePatch = await fetch(`${BASE}/api/meetings/${code}`, {
    method: 'PATCH',
    headers: { 'content-type': 'application/json', cookie: eveCookie },
    body: patch('eve hijack'),
  });
  if (evePatch.status !== 403) fail(`non-owner PATCH returned ${evePatch.status}`);
  console.log('ownership enforced');

  // Cancel from the dashboard; the row flips to the cancelled chip.
  await page.click(`[data-testid="cancel-${code}"]`);
  await page.waitForFunction(
    (c) => document.querySelector(`[data-testid="meeting-row-${c}"]`)?.textContent?.includes('cancelled'),
    { timeout: 15_000 },
    code,
  );
  console.log('cancel from the dashboard works');

  console.log('PASS: magic link and dashboard e2e green');
} catch (error) {
  if (!(error instanceof FailSignal)) {
    console.error('FAIL: unexpected error');
    console.error(error);
    process.exitCode = 1;
  }
} finally {
  await browser.close().catch(() => {});
  server.cleanup();
  if (process.env.DUMP_SERVER_LOGS !== '0') {
    console.log('--- server logs (tail) ---');
    console.log(server.logs().slice(-4000));
  }
}

process.exit(process.exitCode ?? 0);
