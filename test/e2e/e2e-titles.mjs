#!/usr/bin/env node
// Encrypted meeting titles e2e (issue #29, ADR 012): an organizer
// creates an E2EE meeting from the dashboard, the title is sealed
// client-side ("enc1." blob) before it ever reaches the server, and:
//
//   - the organizer's own browser decrypts the title on the dashboard
//     and the prejoin page (organizer-scope key from the ADR 010
//     identity in its own IndexedDB),
//   - the REST API, the ICS export, and the SQLite file at rest only
//     ever carry the masked label and the ciphertext — the plaintext
//     never reaches the disk,
//   - a guest in an isolated browser context (fresh identity key) sees
//     the masked title on prejoin, and the true title only after
//     joining, when it arrives through the E2EE keying relay as a
//     pairwise-sealed title envelope (keying tag 0x05).

import { spawn } from 'node:child_process';
import { mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

const PORT = 3139;
const BASE = `http://127.0.0.1:${PORT}`;
const CHROME = process.env.CHROME_BIN ?? 'google-chrome';
const ROOT = resolve(import.meta.dirname, '../..');
const SERVER = join(ROOT, 'target/debug/huddle');

const TITLE = 'Secret roadmap';
const MASKED = 'Encrypted meeting';

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
  const dataDir = mkdtempSync(join(tmpdir(), 'huddle-titles-e2e-'));
  const server = spawn(SERVER, [], {
    cwd: ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      RUST_LOG: process.env.RUST_LOG ?? 'huddle=info',
      HUDDLE_BIND: `127.0.0.1:${PORT}`,
      // Magic links are built from the base URL — point them here.
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
    dataDir,
    logs: () => logs,
    cleanup: () => {
      server.kill('SIGTERM');
      rmSync(dataDir, { recursive: true, force: true });
    },
  };
}

async function requestMagicLink(email) {
  const res = await fetch(`${BASE}/api/auth/request-link`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ email }),
  });
  if (!res.ok) fail(`request-link for ${email} returned ${res.status}`);
}

async function magicLinkFrom(logs) {
  // Take the LAST logged link: magic links are single-use and earlier
  // ones stay in the log.
  return waitFor('magic link in server logs', async () => {
    const matches = logs().match(/https?:\/\/\S+\/api\/auth\/verify\?token=\S+/g);
    return matches ? matches.at(-1) : null;
  });
}

// datetime-local inputs do not take typed text reliably — build the
// local now±offset string and set the value programmatically.
const localDatetime = (offsetMinutes) => {
  const d = new Date(Date.now() + offsetMinutes * 60_000);
  const pad = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
    `T${pad(d.getHours())}:${pad(d.getMinutes())}`;
};

async function setDatetime(page, testid, value) {
  await page.$eval(`[data-testid="${testid}"]`, (el, v) => {
    el.value = v;
    el.dispatchEvent(new Event('input', { bubbles: true }));
  }, value);
}

async function openPrejoin(page, code, name) {
  await page.goto(`${BASE}/m/${code}?test=1`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-testid="name-input"]');
  await page.type('[data-testid="name-input"]', name);
  await page.waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: media never became ready`));
}

async function prejoinTitle(page) {
  return page.$eval('[data-testid="prejoin-title"]', (el) => el.textContent.trim());
}

async function clickJoin(page) {
  await page.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
}

async function expectInCall(page, who) {
  await page.waitForSelector(`[data-testid="roster-${who}"]`, { timeout: 30_000 })
    .catch(async () => fail(`${who}: never made it into the call`));
  console.log(`${who} is in the call`);
}

async function callTitle(page) {
  return page.$eval('[data-testid="call-title"]', (el) => el.textContent.trim());
}

// Scan every file the server wrote in its data dir (SQLite main file,
// WAL, anything else) — the plaintext title must be in none of them.
function assertNothingAtRest(dataDir) {
  const files = readdirSync(dataDir);
  let sawCiphertext = false;
  for (const file of files) {
    const buf = readFileSync(join(dataDir, file));
    if (buf.includes(TITLE)) {
      fail(`plaintext title "${TITLE}" leaked to disk: ${file}`);
    }
    if (buf.includes('enc1.')) sawCiphertext = true;
  }
  if (!sawCiphertext) fail(`no "enc1." ciphertext found in the data dir: [${files}]`);
  console.log('the data dir holds the ciphertext and no plaintext title');
}

async function main() {
  console.log(`booting server (${SERVER})`);
  const srv = await bootServer();
  let browser;
  const contexts = [];
  try {
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

    // Isolated contexts = isolated IndexedDB = isolated identity keys
    // (ADR 010): the organizer context can decrypt its own titles, the
    // guest context holds a different key and cannot. Pages that must
    // share an identity (dashboard → prejoin → call) share a context.
    const newClient = async (who) => {
      const context = await browser.createBrowserContext();
      contexts.push(context);
      return { context, page: await newPageIn(who, context) };
    };

    const newPageIn = async (who, context) => {
      const page = await context.newPage();
      page.on('console', (m) => {
        if (m.type() !== 'warning') console.log(`[${who} console:${m.type()}]`, m.text());
      });
      page.on('pageerror', (e) => console.log(`[${who} pageerror]`, e.message));
      return page;
    };

    console.log('the organizer signs in with a magic link');
    const { context: organizerContext, page: organizer } = await newClient('organizer');
    await requestMagicLink('organizer@example.com');
    const link = await magicLinkFrom(srv.logs);
    await organizer.goto(link, { waitUntil: 'networkidle0' });
    await organizer.waitForSelector('[data-testid="create-btn"]', { timeout: 15_000 });

    console.log('the organizer creates an e2ee meeting — the title is sealed client-side');
    await organizer.type('[data-testid="create-title"]', TITLE);
    // Start the meeting in the past so it is open for joining right away
    // (a future start would bounce the join with a NotYet error).
    await setDatetime(organizer, 'create-starts', localDatetime(-2));
    await setDatetime(organizer, 'create-ends', localDatetime(60));
    await organizer.click('[data-testid="create-e2ee"]');
    await organizer.click('[data-testid="create-btn"]');

    const row = await organizer.waitForSelector('[data-testid^="meeting-row-"]', { timeout: 15_000 })
      .catch(async () => fail('the created meeting never appeared on the dashboard'));
    const rowTestid = await row.evaluate((el) => el.getAttribute('data-testid'));
    const code = rowTestid.replace('meeting-row-', '');
    console.log(`created meeting ${code} from the dashboard`);

    console.log('the organizer sees the DECRYPTED title on the dashboard');
    const dashTitle = await waitFor(
      'decrypted dashboard title',
      () => organizer.$eval(`[data-testid="meeting-title-${code}"]`, (el) => el.textContent.trim())
        .then((t) => (t === TITLE ? t : null))
        .catch(() => null),
    );
    if (dashTitle !== TITLE) fail(`dashboard title was "${dashTitle}"`);

    console.log('the API masks the title and only ships the ciphertext');
    const api = await fetch(`${BASE}/api/meetings/${code}`);
    const meeting = (await api.json()).meeting;
    if (api.status !== 200) fail(`GET meeting returned ${api.status}`);
    if (meeting.title !== MASKED) {
      fail(`API title is "${meeting.title}", want the masked "${MASKED}"`);
    }
    const blob = meeting.title_ciphertext;
    if (typeof blob !== 'string' || !blob.startsWith('enc1.')) {
      fail(`API did not carry the ciphertext: ${JSON.stringify(blob)}`);
    }
    if (blob.includes(TITLE)) fail('the ciphertext carries the plaintext?!');

    console.log('the ICS export carries the masked title only');
    const ics = await fetch(`${BASE}/api/meetings/${code}/ics`);
    const icsBody = await ics.text();
    if (!icsBody.includes(`SUMMARY:${MASKED}`)) {
      fail(`ICS SUMMARY is not the masked title: ${icsBody.split('\n').find((l) => l.startsWith('SUMMARY'))}`);
    }
    if (icsBody.includes(TITLE)) fail('ICS export leaks the plaintext title');
    if (icsBody.includes(blob)) fail('ICS export carries the ciphertext blob');

    console.log('nothing at rest in the data dir holds the plaintext title');
    assertNothingAtRest(srv.dataDir);

    console.log('the organizer prejoin decrypts the title (same identity)');
    const organizerPrejoin = await newPageIn('organizer', organizerContext);
    await openPrejoin(organizerPrejoin, code, 'organizer');
    const organizerPrejoinTitle = await prejoinTitle(organizerPrejoin);
    if (organizerPrejoinTitle !== TITLE) {
      fail(`organizer prejoin title is "${organizerPrejoinTitle}", want "${TITLE}"`);
    }

    console.log('a guest with a different identity sees the masked prejoin title');
    const { page: guest } = await newClient('guest');
    await openPrejoin(guest, code, 'guest');
    const guestPrejoinTitle = await prejoinTitle(guest);
    if (guestPrejoinTitle !== MASKED) {
      fail(`guest prejoin title is "${guestPrejoinTitle}", want "${MASKED}"`);
    }

    console.log('after joining, the true title reaches the guest via the e2ee relay');
    await clickJoin(organizerPrejoin);
    await expectInCall(organizerPrejoin, 'organizer');
    await clickJoin(guest);
    await expectInCall(guest, 'guest');
    await organizerPrejoin.waitForSelector('[data-testid="roster-guest"]', { timeout: 30_000 })
      .catch(async () => fail('organizer never saw the guest join'));

    const guestCallTitle = await waitFor(
      'the guest call header to show the true title',
      async () => {
        const t = await callTitle(guest);
        return t === TITLE ? t : null;
      },
    );
    if (guestCallTitle !== TITLE) fail(`guest call title was "${guestCallTitle}"`);
    const organizerCallTitle = await callTitle(organizerPrejoin);
    if (organizerCallTitle !== TITLE) {
      fail(`organizer call title is "${organizerCallTitle}", want "${TITLE}"`);
    }

    console.log('PASS: titles are ciphertext at rest, masked for other clients, true after join');
  } finally {
    for (const context of contexts) await context.close().catch(() => {});
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
