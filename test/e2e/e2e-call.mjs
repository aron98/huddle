#!/usr/bin/env node
// Two-person end-to-end call test.
//
// Boots a real huddle server, opens the meeting in two headless Chrome
// pages with fake cameras/microphones, and asserts that each page
// receives the other's audio *and* video through the SFU (there is no
// peer-to-peer path at all), and that mute state broadcasts propagate.

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

const PORT = 3131;
const BASE = `http://127.0.0.1:${PORT}`;
const CHROME = process.env.CHROME_BIN ?? 'google-chrome';
const ROOT = resolve(import.meta.dirname, '../..');
const SERVER = join(ROOT, 'target/debug/huddle');

const fail = (msg) => {
  console.error(`FAIL: ${msg}`);
  process.exit(1);
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
  const dataDir = mkdtempSync(join(tmpdir(), 'huddle-e2e-'));
  const server = spawn(SERVER, [], {
    cwd: ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
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

async function createMeeting() {
  const rfc3339 = (date) => date.toISOString().replace(/\.\d+Z$/, 'Z');
  const res = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: 'e2e call',
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
  await page.click('[data-testid="join-btn"]');
  // The prejoin form goes away once the call view mounts.
  await page
    .waitForSelector('[data-testid="name-input"]', { hidden: true, timeout: 30_000 })
    .catch(async () => fail(`join as ${name} never left the prejoin page (status: ${await statusText(page)})`));
}

// Remote media snapshot: does the tile exist, is a stream attached, and
// how many inbound audio/video bytes have arrived through the SFU?
async function mediaProbe(page, peer) {
  return page.evaluate(async (peerName) => {
    const tile = document.querySelector(`[data-testid="tile-${peerName}"] video`);
    const pc = window.__huddlePc;
    if (!tile || !pc) return { tile: false };
    const stream = tile.srcObject;
    const stats = await pc.getStats();
    let audioBytes = 0;
    let videoBytes = 0;
    stats.forEach((report) => {
      if (report.type === 'inbound-rtp') {
        if (report.kind === 'audio') audioBytes = report.bytesReceived ?? 0;
        if (report.kind === 'video') videoBytes = report.bytesReceived ?? 0;
      }
    });
    return {
      tile: true,
      hasStream: !!stream,
      videoTracks: stream ? stream.getVideoTracks().length : 0,
      audioTracks: stream ? stream.getAudioTracks().length : 0,
      audioBytes,
      videoBytes,
      connection: pc.connectionState,
    };
  }, peer);
}

async function main() {
  console.log(`booting server (${SERVER})`);
  const srv = await bootServer();
  let browser;
  try {
    const code = await createMeeting();
    console.log(`meeting ${code} created, launching chrome (${CHROME})`);
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

    const alice = await browser.newPage();
    const bob = await browser.newPage();

    console.log('alice and bob join the meeting');
    await joinAs(alice, code, 'alice');
    await joinAs(bob, code, 'bob');

    console.log('waiting for tiles');
    await alice.waitForSelector('[data-testid="tile-bob"]', { timeout: 30_000 });
    await bob.waitForSelector('[data-testid="tile-alice"]', { timeout: 30_000 });

    console.log('asserting media flows through the server');
    for (const [page, peer, who] of [[alice, 'bob', 'alice'], [bob, 'alice', 'bob']]) {
      const m = await waitFor(
        `${who} receiving ${peer}'s media`,
        async () => {
          const p = await mediaProbe(page, peer);
          return p.hasStream && p.audioBytes > 0 && p.videoBytes > 0 ? p : null;
        },
        60_000,
      );
      console.log(`${who} <- ${peer}: ${JSON.stringify(m)}`);
    }

    console.log('asserting mute state propagates');
    await alice.click('[data-testid="toggle-mic"]');
    await bob.waitForFunction(
      () => document.querySelector('[data-testid="roster-alice"]')?.textContent.includes('mic off'),
      { timeout: 30_000, polling: 250 },
    );
    console.log('bob sees alice (mic off, cam on)');

    console.log('PASS: two-person call e2e green');
  } catch (err) {
    fail(`unexpected error: ${err?.stack ?? err}`);
  } finally {
    await browser?.close();
    srv.cleanup();
  }
}

main();
