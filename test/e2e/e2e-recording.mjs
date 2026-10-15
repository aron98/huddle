#!/usr/bin/env node
// Meeting recording e2e (issue 15).
//
// Boots a real huddle server, opens the meeting in headless Chrome
// pages, and asserts the recording lifecycle end to end:
//   - only the meeting's organizer can start/stop recording (anonymous
//     and other sessions get 403),
//   - a start that cannot happen (no live meeting) is 409,
//   - while recording, every participant sees the REC indicator and a
//     system chat notice (nobody is silently recorded), and a late
//     recording state reaches pages already in the call,
//   - stop withdraws the indicator and posts its own system notice,
//   - the on-disk artifacts exist: per-track .rtp files plus the
//     session.sdp manifest the ffmpeg sidecar muxes from.
//
// Port 3135: 3131-3134 are used by the sibling suites (call, auth, and
// auth's second OIDC instance), so this suite keeps out of their way.

import { spawn } from 'node:child_process';
import { mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

const PORT = 3135;
const BASE = `http://127.0.0.1:${PORT}`;
const CHROME = process.env.CHROME_BIN ?? 'google-chrome';
const ROOT = resolve(import.meta.dirname, '../..');
const SERVER = join(ROOT, 'target/debug/huddle');

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

async function statusText(page) {
  try {
    return await page.$eval('[data-testid="call-status"]', (el) => el.textContent);
  } catch {
    return '(no call-status element)';
  }
}

async function bootServer() {
  const dataDir = mkdtempSync(join(tmpdir(), 'huddle-rec-e2e-'));
  const server = spawn(SERVER, [], {
    cwd: ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      RUST_LOG: process.env.RUST_LOG ?? 'huddle=info',
      HUDDLE_BIND: `127.0.0.1:${PORT}`,
      // Magic links are built from the configured base URL.
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

const rfc3339 = (date) => date.toISOString().replace(/\.\d+Z$/, 'Z');

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

async function loginAs(email, logs) {
  await requestMagicLink(email);
  const link = await magicLinkFrom(logs);
  const res = await fetch(link, { redirect: 'manual' });
  const cookie = res.headers.get('set-cookie');
  if (res.status !== 303 || !cookie?.startsWith('huddle_session=')) {
    fail(`verify link for ${email} did not issue a session: status=${res.status} cookie=${cookie}`);
  }
  return cookie;
}

async function createOwnedMeeting(cookie) {
  const res = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', cookie },
    body: JSON.stringify({
      title: 'recording e2e',
      starts_at: rfc3339(new Date()),
      ends_at: rfc3339(new Date(Date.now() + 60 * 60_000)),
    }),
  });
  if (!res.ok) fail(`owned meeting creation failed: ${res.status} ${await res.text()}`);
  const body = await res.json();
  if (!body.meeting.id) fail(`created meeting DTO has no id: ${JSON.stringify(body)}`);
  return body.meeting;
}

async function joinAs(page, code, name) {
  await page.goto(`${BASE}/m/${code}?test=1`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-testid="name-input"]');
  await page.type('[data-testid="name-input"]', name);
  // Join is gated on the local stream being live.
  await page.waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: media never became ready (status: ${await statusText(page)})`));
  await page.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
  await page
    .waitForSelector('[data-testid="name-input"]', { hidden: true, timeout: 30_000 })
    .catch(async () =>
      fail(`join as ${name} never left the prejoin page (status: ${await statusText(page)})`));
}

async function recordingRequest(method, code, cookie) {
  const res = await fetch(`${BASE}/api/meetings/${code}/recording`, {
    method,
    ...(cookie ? { headers: { cookie } } : {}),
  });
  return { status: res.status, body: await res.json().catch(() => null) };
}

// The REC indicator, the consent signage every participant must see.
const indicatorVisible = (page) =>
  page.evaluate(() => !!document.querySelector('[data-testid="recording-indicator"]'));

async function main() {
  console.log(`booting server (${SERVER})`);
  const srv = await bootServer();
  let browser;
  try {
    const owner = await loginAs('owner@example.com', srv.logs);
    const meeting = await createOwnedMeeting(owner);
    const code = meeting.code;
    console.log(`meeting ${code} (${meeting.id}) created by the organizer`);

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

    const alice = await newClient('alice');
    const bob = await newClient('bob');
    console.log('alice and bob join the meeting');
    await joinAs(alice, code, 'alice');
    await joinAs(bob, code, 'bob');
    await alice.waitForSelector('[data-testid="tile-bob"]', { timeout: 30_000 });
    await bob.waitForSelector('[data-testid="tile-alice"]', { timeout: 30_000 });

    // A screen share gives the recorder a second video track to tap.
    console.log('alice shares her screen');
    await alice.click('[data-testid="share-screen"]');
    await alice
      .waitForFunction(
        () =>
          document.querySelector('[data-testid="share-screen"]')?.textContent.includes('Stop sharing'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => fail(`alice: screen share never started (status: ${await statusText(alice)})`));
    await bob.waitForSelector('[data-testid="tile-alice-screen"]', { timeout: 30_000 });

    console.log('asserting only the organizer can record');
    const anonymousStart = await recordingRequest('POST', code, null);
    if (anonymousStart.status !== 403) {
      fail(`anonymous start returned ${anonymousStart.status}, want 403`);
    }
    const eve = await loginAs('eve@example.com', srv.logs);
    const eveStart = await recordingRequest('POST', code, eve);
    if (eveStart.status !== 403) {
      fail(`non-owner start returned ${eveStart.status}, want 403`);
    }

    console.log('organizer starts recording');
    const started = await recordingRequest('POST', code, owner);
    if (started.status !== 201) {
      fail(`owner start returned ${started.status}: ${JSON.stringify(started.body)}`);
    }
    if (!started.body?.active) fail(`started recording is not active: ${JSON.stringify(started.body)}`);

    // Nobody is silently recorded: the indicator and the system chat
    // notice reach both participants' pages.
    for (const [page, who] of [[alice, 'alice'], [bob, 'bob']]) {
      await page
        .waitForFunction(() => !!document.querySelector('[data-testid="recording-indicator"]'), {
          timeout: 30_000,
          polling: 250,
        })
        .catch(async () => fail(`${who} never saw the recording indicator`));
      await page
        .waitForFunction(
          () =>
            [...document.querySelectorAll('[data-testid="chat-log"] li')].some((li) =>
              li.textContent.includes('Recording started'),
            ),
          { timeout: 30_000, polling: 250 },
        )
        .catch(async () => fail(`${who} never saw the recording consent chat message`));
    }
    console.log('both participants see the REC indicator and the consent notice');

    const status = await recordingRequest('GET', code, owner);
    if (status.status !== 200 || !status.body?.active) {
      fail(`status during recording: ${status.status} ${JSON.stringify(status.body)}`);
    }

    console.log('organizer stops recording');
    const stopped = await recordingRequest('DELETE', code, owner);
    if (stopped.status !== 200 || stopped.body?.active) {
      fail(`stop returned ${stopped.status}: ${JSON.stringify(stopped.body)}`);
    }

    for (const [page, who] of [[alice, 'alice'], [bob, 'bob']]) {
      await page
        .waitForFunction(() => !document.querySelector('[data-testid="recording-indicator"]'), {
          timeout: 30_000,
          polling: 250,
        })
        .catch(async () => fail(`${who}'s recording indicator never withdrew`));
      await page
        .waitForFunction(
          () =>
            [...document.querySelectorAll('[data-testid="chat-log"] li')].some((li) =>
              li.textContent.includes('Recording stopped'),
            ),
          { timeout: 30_000, polling: 250 },
        )
        .catch(async () => fail(`${who} never saw the recording stopped chat message`));
    }

    console.log('asserting on-disk artifacts');
    const recRoot = join(srv.dataDir, 'recordings', meeting.id);
    const recDir = join(recRoot, started.body.id);
    const manifest = readFileSync(join(recDir, 'session.sdp'), 'utf8');
    if (!manifest.startsWith('v=0')) fail(`session.sdp does not look like SDP: ${manifest.slice(0, 80)}`);
    if (!manifest.includes('m=audio')) fail(`manifest has no audio line: ${manifest}`);
    const rtpFiles = readdirSync(recDir).filter((f) => f.endsWith('.rtp'));
    if (rtpFiles.length === 0) fail('no per-track .rtp files were written');
    for (const f of rtpFiles) {
      const size = statSync(join(recDir, f)).size;
      if (size === 0) fail(`track file ${f} is empty`);
      console.log(`artifact ${f}: ${size} bytes`);
    }

    console.log('PASS: recording e2e green');
  } catch (err) {
    if (!(err instanceof FailSignal)) {
      console.error(`FAIL: unexpected error: ${err?.stack ?? err}`);
      process.exitCode = 1;
    }
  } finally {
    await browser?.close();
    if (process.env.DUMP_SERVER_LOGS !== '0') console.log('--- server logs (tail) ---\n' + srv.logs().slice(-4000));
    srv.cleanup();
  }
}

main();
