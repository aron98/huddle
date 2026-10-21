#!/usr/bin/env node
// E2EE key exchange end-to-end test (issue #27, ADR 010): a meeting
// created with the E2EE trust mode flags itself in the UI, two
// participants in isolated browser contexts (so each gets its own
// IndexedDB identity) derive the SAME pairwise safety number on both
// sides, and confirming the comparison on one side propagates the
// verified mark to the other through the encrypted relay.
//
// What makes this assertable where decryption is not (ADR 009): the
// safety number is a pure function of BOTH identity keys — it can only
// match if the two browsers hold the same view of each other's public
// keys, i.e. the server relayed the announces faithfully.

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

const PORT = 3137;
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

async function createE2eeMeeting() {
  const rfc3339 = (date) => date.toISOString().replace(/\.\d+Z$/, 'Z');
  const res = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: 'e2e encrypted',
      starts_at: rfc3339(new Date()),
      ends_at: rfc3339(new Date(Date.now() + 60 * 60 * 1000)),
      e2ee: true,
    }),
  });
  if (!res.ok) fail(`e2ee meeting creation failed: ${res.status} ${await res.text()}`);
  const meeting = (await res.json()).meeting;
  if (meeting.e2ee !== true) fail(`created meeting has no e2ee flag: ${JSON.stringify(meeting)}`);
  console.log(`meeting ${meeting.code} created with the e2ee trust mode`);
  return meeting.code;
}

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

async function expectInCall(page, who) {
  await page.waitForSelector(`[data-testid="roster-${who}"]`, { timeout: 30_000 })
    .catch(async () => fail(`${who}: never made it into the call`));
  console.log(`${who} is in the call`);
}

async function e2eeStats(page) {
  return page.evaluate(() => window.__huddleE2eeStats ?? null);
}

async function peerFromStats(page, name) {
  const stats = await e2eeStats(page);
  if (!stats || !Array.isArray(stats.peers)) return null;
  return stats.peers.find((p) => p.name === name) ?? null;
}

async function main() {
  console.log(`booting server (${SERVER})`);
  const srv = await bootServer();
  let browser;
  const contexts = [];
  try {
    const code = await createE2eeMeeting();

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

    // Each participant gets an isolated browser context: the identity
    // keypair lives in IndexedDB, which is shared per context — without
    // isolation both pages would load the SAME identity and the test
    // would prove nothing about pairwise derivation.
    const newClient = async (who) => {
      const context = await browser.createBrowserContext();
      contexts.push(context);
      const page = await context.newPage();
      page.on('console', (m) => {
        if (m.type() !== 'warning') console.log(`[${who} console:${m.type()}]`, m.text());
      });
      page.on('pageerror', (e) => console.log(`[${who} pageerror]`, e.message));
      return page;
    };

    console.log('alice and bob join the encrypted meeting');
    const alice = await newClient('alice');
    const bob = await newClient('bob');
    await openPrejoin(alice, code, 'alice');
    await clickJoin(alice);
    await expectInCall(alice, 'alice');
    await openPrejoin(bob, code, 'bob');
    await clickJoin(bob);
    await expectInCall(bob, 'bob');
    // Bob's join shows on alice's roster too.
    await alice.waitForSelector('[data-testid="roster-bob"]', { timeout: 30_000 })
      .catch(async () => fail('alice never saw bob join'));

    console.log('the trust mode is visible in the call header on both sides');
    await alice.waitForSelector('[data-testid="e2ee-indicator"]', { timeout: 30_000 })
      .catch(async () => fail('alice: the e2ee indicator never showed'));
    await bob.waitForSelector('[data-testid="e2ee-indicator"]', { timeout: 30_000 })
      .catch(async () => fail('bob: the e2ee indicator never showed'));

    console.log('both sides complete the key exchange and offer verification');
    // The Verify button only appears once the pairwise key (and thus
    // the safety number) is derived — this IS the keying round-trip.
    await alice.waitForSelector('[data-testid="verify-bob"]', { timeout: 30_000 })
      .catch(async () => fail('alice: the verify-bob affordance never appeared'));
    await bob.waitForSelector('[data-testid="verify-alice"]', { timeout: 30_000 })
      .catch(async () => fail('bob: the verify-alice affordance never appeared'));

    console.log('both sides show the SAME safety number');
    await alice.click('[data-testid="verify-bob"]');
    await alice.waitForSelector('[data-testid="verify-dialog"]', { timeout: 10_000 })
      .catch(async () => fail('alice: the verify dialog never opened'));
    await bob.click('[data-testid="verify-alice"]');
    await bob.waitForSelector('[data-testid="verify-dialog"]', { timeout: 10_000 })
      .catch(async () => fail('bob: the verify dialog never opened'));
    const aliceNumber = await alice.$eval(
      '[data-testid="safety-number-bob"]', (el) => el.textContent.trim());
    const bobNumber = await bob.$eval(
      '[data-testid="safety-number-alice"]', (el) => el.textContent.trim());
    console.log(`alice sees ${aliceNumber}, bob sees ${bobNumber}`);
    if (!/^\d{3} \d{3} \d{3} \d{3}$/.test(aliceNumber)) {
      fail(`alice's safety number is not 12 digits in 4 triplets: "${aliceNumber}"`);
    }
    if (aliceNumber !== bobNumber) {
      fail(`the safety numbers differ: alice "${aliceNumber}" vs bob "${bobNumber}"`);
    }

    console.log('the probe agrees with the dialogs on both sides');
    const alicePeer = await waitFor('alice probe to list bob', () => peerFromStats(alice, 'bob'));
    const bobPeer = await waitFor('bob probe to list alice', () => peerFromStats(bob, 'alice'));
    for (const [who, peer] of [['alice', alicePeer], ['bob', bobPeer]]) {
      if (peer.safetyNumber !== aliceNumber) {
        fail(`${who}: probe safety number "${peer.safetyNumber}" != dialog "${aliceNumber}"`);
      }
      if (peer.verified !== false) {
        fail(`${who}: peer starts verified? ${JSON.stringify(peer)}`);
      }
    }
    const aliceStats = await e2eeStats(alice);
    const bobStats = await e2eeStats(bob);
    if (aliceStats.enabled !== true || bobStats.enabled !== true) fail('probe says e2ee is off');
    if (aliceStats.epoch < 1 || bobStats.epoch < 1) fail('probe reports no epoch');
    if (aliceStats.epoch !== bobStats.epoch) {
      fail(`epochs drifted: alice ${aliceStats.epoch} vs bob ${bobStats.epoch}`);
    }
    if (aliceStats.hasSenderKey !== true || bobStats.hasSenderKey !== true) {
      fail('probe reports no sender key on a side');
    }
    console.log(`e2ee active on both sides at epoch ${aliceStats.epoch}`);

    console.log('alice marks the comparison as verified');
    await alice.click('[data-testid="confirm-verify-bob"]');
    await alice.waitForSelector('[data-testid="verified-bob"]', { timeout: 10_000 })
      .catch(async () => fail('alice: the verified chip never showed'));
    console.log('alice recorded bob as verified');

    console.log('bob converges through the relayed confirmation — no action on his side');
    await bob.waitForSelector('[data-testid="verified-alice"]', { timeout: 30_000 })
      .catch(async () => fail('bob: the verified chip never showed after alice confirmed'));
    const bobPeerAfter = await waitFor(
      'bob probe to report verified',
      async () => {
        const peer = await peerFromStats(bob, 'alice');
        return peer?.verified ? peer : null;
      },
    );
    console.log(`bob's probe reports verified=${bobPeerAfter.verified}`);
    const alicePeerAfter = await waitFor(
      'alice probe to report verified',
      async () => {
        const peer = await peerFromStats(alice, 'bob');
        return peer?.verified ? peer : null;
      },
    );
    if (alicePeerAfter.safetyNumber !== bobPeerAfter.safetyNumber) {
      fail('safety numbers changed across the verification flow');
    }

    console.log('PASS: same safety number on both sides, verification propagates via the relay');
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
