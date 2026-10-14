#!/usr/bin/env node
// N-party end-to-end call test with join/leave churn.
//
// Boots a real huddle server, opens the meeting in headless Chrome
// pages with fake cameras/microphones, and asserts that each page
// receives the others' audio *and* video through the SFU (there is no
// peer-to-peer path at all), that a late joiner gets fresh media
// rather than frozen tiles, that a departing participant's tile and
// roster entry disappear everywhere, and that mute state broadcasts
// propagate.

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
  // Join is gated on the local stream being live (the button enables
  // once getUserMedia has produced tracks) — wait for that, not just
  // for the button to exist.
  await page.waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: media never became ready (status: ${await statusText(page)})`));
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
    // One inbound-rtp report per ssrc: sum across every remote peer.
    stats.forEach((report) => {
      if (report.type === 'inbound-rtp') {
        if (report.kind === 'audio') audioBytes += report.bytesReceived ?? 0;
        if (report.kind === 'video') videoBytes += report.bytesReceived ?? 0;
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

const stateDump = (page, who) =>
  page
    .evaluate(() => {
      const pc = window.__huddlePc;
      return {
        status: document.querySelector('[data-testid="call-status"]')?.textContent ?? null,
        tiles: [...document.querySelectorAll('[data-testid^="tile-"]')].map((e) => e.dataset.testid),
        roster: [...document.querySelectorAll('[data-testid^="roster-"]')].map((e) => e.textContent),
        pc: pc
          ? {
              conn: pc.connectionState,
              sig: pc.signalingState,
              localM: pc.localDescription?.sdp.split('\n').filter((l) => l.startsWith('m=')).length,
              remoteM: pc.remoteDescription?.sdp.split('\n').filter((l) => l.startsWith('m=')).length,
            }
          : null,
      };
    })
    .then((s) => console.log(`[${who} state]`, JSON.stringify(s)))
    .catch((e) => console.log(`[${who} state] probe failed:`, e.cause?.message ?? e.message));

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

    // Every participant gets a console/pageerror relay into the job log.
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

    console.log('waiting for tiles');
    const waitTile = async (page, sel, who) => {
      try {
        await page.waitForSelector(sel, { timeout: 30_000 });
      } catch (e) {
        await stateDump(page, who).catch(() => {});
        const status = await statusText(page).catch(() => '(status probe failed)');
        fail(`${who}: tile wait ${sel} failed; status: ${status}`);
      }
    };
    await waitTile(alice, '[data-testid="tile-bob"]', 'alice');
    await waitTile(bob, '[data-testid="tile-alice"]', 'bob');

    console.log('asserting media flows through the server');
    const waitMedia = async (page, peer, who) => {
      let probe = null;
      await waitFor(
        `${who} receiving ${peer}'s media`,
        async () => {
          probe = await mediaProbe(page, peer);
          return probe.hasStream && probe.audioBytes > 0 && probe.videoBytes > 0;
        },
        60_000,
      ).catch(async () => {
        await stateDump(page, who).catch(() => {});
        fail(`${who}: no media arriving from ${peer}: ${JSON.stringify(probe)}`);
      });
      console.log(`${who} <- ${peer}: ${JSON.stringify(probe)}`);
    };
    await waitMedia(alice, 'bob', 'alice');
    await waitMedia(bob, 'alice', 'bob');

    console.log('carol joins late');
    const carol = await newClient('carol');
    await joinAs(carol, code, 'carol');

    // A late joiner must get a live view of the existing participants,
    // not a frozen tile: the keyframe fan-out should make their video
    // start decoding immediately.
    await waitTile(carol, '[data-testid="tile-alice"]', 'carol');
    await waitTile(carol, '[data-testid="tile-bob"]', 'carol');
    await waitTile(alice, '[data-testid="tile-carol"]', 'alice');
    await waitTile(bob, '[data-testid="tile-carol"]', 'bob');
    let carolProbe = null;
    await waitFor(
      'carol receiving existing media',
      async () => {
        carolProbe = await mediaProbe(carol, 'alice');
        return carolProbe.hasStream && carolProbe.audioBytes > 0 && carolProbe.videoBytes > 0;
      },
      60_000,
    ).catch(async () => {
      await stateDump(carol, 'carol').catch(() => {});
      fail(`carol: no media arriving from existing participants: ${JSON.stringify(carolProbe)}`);
    });
    console.log(`carol <- alice+bob: ${JSON.stringify(carolProbe)}`);

    console.log('dave joins, is seen, then leaves');
    const dave = await newClient('dave');
    await joinAs(dave, code, 'dave');
    await waitTile(alice, '[data-testid="tile-dave"]', 'alice');
    await waitTile(bob, '[data-testid="tile-dave"]', 'bob');
    await waitTile(carol, '[data-testid="tile-dave"]', 'carol');
    for (const who of ['alice', 'bob', 'carol']) {
      await waitTile(dave, `[data-testid="tile-${who}"]`, 'dave');
    }

    await dave.click('[data-testid="leave-btn"]');
    await dave.waitForFunction(
      () => (document.querySelector('[data-testid="call-status"]')?.textContent ?? '').includes('left'),
      { timeout: 30_000, polling: 250 },
    );
    // Dave's tile and roster entry must vanish on every remaining page.
    for (const [page, who] of [
      [alice, 'alice'],
      [bob, 'bob'],
      [carol, 'carol'],
    ]) {
      await page.waitForFunction(
        () =>
          !document.querySelector('[data-testid="tile-dave"]') &&
          !document.querySelector('[data-testid="roster-dave"]'),
        { timeout: 30_000, polling: 250 },
      ).catch(async (e) => {
        await stateDump(page, who).catch(() => {});
        fail(`${who}: dave never disappeared after leaving`);
      });
    }
    console.log('dave left; tiles and roster entries cleaned up everywhere');

    console.log('asserting mute state propagates');
    await alice.click('[data-testid="toggle-mic"]');
    await bob.waitForFunction(
      () => document.querySelector('[data-testid="roster-alice"]')?.textContent.includes('mic off'),
      { timeout: 30_000, polling: 250 },
    );
    console.log('bob sees alice (mic off, cam on)');

    console.log('PASS: n-party call e2e green');
  } catch (err) {
    if (!(err instanceof FailSignal)) {
      console.error(`FAIL: unexpected error: ${err?.stack ?? err}`);
      process.exitCode = 1;
    }
  } finally {
    await browser?.close();
    if (process.env.DUMP_SERVER_LOGS !== '0') console.log('--- server logs:\n' + srv.logs());
    srv.cleanup();
  }
}

main();
