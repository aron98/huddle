#!/usr/bin/env node
// HTTPS-origin signaling e2e (issue #65).
//
// The client used to hard-code ws:// for the signaling URL, so every
// HTTPS deployment failed on mixed content — and nothing caught it,
// because the whole e2e farm runs plain HTTP. This suite stands up the
// server with built-in TLS (static self-signed certificate, ADR 008),
// points headless Chrome at the https:// origin, and joins a meeting.
// Reaching the call view proves the client derived wss:// from the
// page protocol and the server's /ws rode the TLS listener.

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

// The Node-side fetches (health, meeting creation) talk to a
// self-signed origin; the certificate check is not what's under test.
process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';

const PORT = 3138;
const BASE = `https://127.0.0.1:${PORT}`;
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

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

async function statusText(page) {
  try {
    return await page.$eval('[data-testid="call-status"]', (el) => el.textContent);
  } catch {
    return '(no call-status element)';
  }
}

// Generate a throwaway self-signed certificate for 127.0.0.1 (with an
// IP SAN, not just a CN) the same way an operator would for a static
// TLS deployment — the server's HUDDLE_TLS_CERT/HUDDLE_TLS_KEY mode.
function generateCerts(dir) {
  const cert = join(dir, 'cert.pem');
  const key = join(dir, 'key.pem');
  const openssl = spawn('/usr/bin/openssl', [
    'req',
    '-x509',
    '-newkey',
    'rsa:2048',
    '-nodes',
    '-keyout', key,
    '-out', cert,
    '-days', '1',
    '-subj', '/CN=127.0.0.1',
    '-addext', 'subjectAltName=IP:127.0.0.1',
  ]);
  let err = '';
  openssl.stderr.on('data', (d) => (err += d));
  return new Promise((res, rej) => {
    openssl.on('close', (code) => (code === 0 ? res({ cert, key }) : rej(new Error(err))));
  });
}

async function bootServer(cert, key) {
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
      HUDDLE_TLS_CERT: cert,
      HUDDLE_TLS_KEY: key,
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

async function createMeeting() {
  const rfc3339 = (date) => date.toISOString().replace(/\.\d+Z$/, 'Z');
  const res = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: 'e2e wss signaling',
      starts_at: rfc3339(new Date()),
      ends_at: rfc3339(new Date(Date.now() + 60 * 60 * 1000)),
    }),
  });
  if (!res.ok) fail(`meeting creation failed: ${res.status} ${await res.text()}`);
  const body = await res.json();
  return body.meeting.code;
}

async function joinAs(page, code, name) {
  await page.goto(`${BASE}/m/${code}?test=1`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-testid="name-input"]');
  await page.type('[data-testid="name-input"]', name);
  // Join is gated on the local stream being live — wait for the
  // button to be enabled, and click the element itself (a coordinate
  // click races the preview's first-frame reflow).
  await page.waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: media never became ready (status: ${await statusText(page)})`));
  await page.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
  // Leaving the prejoin page means the wss:// signaling connection
  // connected AND the join handshake completed — this is the fix
  // under test. On the old ws:// hard-code this times out with a
  // mixed-content block (JoinError::Unreachable).
  await page
    .waitForSelector('[data-testid="name-input"]', { hidden: true, timeout: 30_000 })
    .catch(async () => {
      const stuck = await page
        .evaluate(() => ({
          joinDisabled: document.querySelector('[data-testid="join-btn"]')?.disabled ?? null,
          name: document.querySelector('[data-testid="name-input"]')?.value ?? null,
        }))
        .catch(() => null);
      fail(
        `join as ${name} never left the prejoin page over HTTPS ` +
          `(status: ${await statusText(page)}, prejoin: ${JSON.stringify(stuck)})`,
      );
    });
}

async function main() {
  console.log(`generating self-signed cert and booting TLS server (${SERVER})`);
  const certDir = mkdtempSync(join(tmpdir(), 'huddle-e2e-cert-'));
  let srv;
  let browser;
  try {
    const { cert, key } = await generateCerts(certDir);
    srv = await bootServer(cert, key);
    const code = await createMeeting();
    console.log(`meeting ${code} created over HTTPS, launching chrome (${CHROME})`);
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
        // The server's leaf is self-signed; the suite tests the wss
        // signaling path, not the certificate chain.
        '--ignore-certificate-errors',
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

    console.log('alice and bob join over wss://');
    await joinAs(alice, code, 'alice');
    await joinAs(bob, code, 'bob');

    console.log('waiting for tiles (roster fan-out over the TLS connection)');
    for (const [page, sel, who] of [
      [alice, '[data-testid="tile-bob"]', 'alice'],
      [bob, '[data-testid="tile-alice"]', 'bob'],
    ]) {
      await page.waitForSelector(sel, { timeout: 30_000 }).catch(async () => {
        fail(`${who}: tile wait ${sel} failed over HTTPS (status: ${await statusText(page)})`);
      });
    }

    console.log('PASS: https-origin signaling e2e green');
  } catch (err) {
    if (!(err instanceof FailSignal)) {
      console.error(`FAIL: unexpected error: ${err?.stack ?? err}`);
      process.exitCode = 1;
    }
  } finally {
    await browser?.close();
    if (process.env.DUMP_SERVER_LOGS !== '0' && srv) console.log('--- server logs:\n' + srv.logs());
    srv?.cleanup();
    rmSync(certDir, { recursive: true, force: true });
  }
}

main();
