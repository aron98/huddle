#!/usr/bin/env node
// E2EE end-to-end tests (issues #27 + #28, ADR 010/011).
//
// Scenario 1 (key exchange, #27): a meeting created with the E2EE
// trust mode flags itself in the UI, two participants in isolated
// browser contexts (so each gets its own IndexedDB identity) derive
// the SAME pairwise safety number on both sides, and confirming the
// comparison on one side propagates the verified mark to the other
// through the encrypted relay. What makes this assertable where raw
// key material is not (ADR 009): the safety number is a pure function
// of BOTH identity keys.
//
// Scenario 1 continues (#28): with the SFrame media transform worker
// live on every sender and receiver, media still flows (inbound-rtp
// framesDecoded/bytesReceived grow through the transforms), the probe
// reports real encrypt/decrypt counters with zero decrypt failures,
// chat bodies round-trip as ciphertext-encrypted-but-readable, and a
// LATE joiner sees only the placeholder for earlier messages —
// proving chat persists as ciphertext server-side (ADR 011, D7/D8).
//
// Scenario 2 (#28, D5): the `?e2eeDegrade=1` knob forces the worker's
// EMA over the 20ms threshold, tripping the one-shot degrade notice
// and the probe's degraded flag.
//
// Scenario 3 (#28, D10): a PLAINTEXT meeting with `?e2eeTag=1` runs
// the transform plumbing in tag-only mode — media flows through
// transforms and the SFU without real crypto, pinning the whole
// attach/forward/strip path independently of WebCrypto.

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

async function createMeeting(e2ee) {
  const rfc3339 = (date) => date.toISOString().replace(/\.\d+Z$/, 'Z');
  const res = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: e2ee ? 'e2e encrypted' : 'e2e tag-mode plumbing',
      starts_at: rfc3339(new Date()),
      ends_at: rfc3339(new Date(Date.now() + 60 * 60 * 1000)),
      e2ee,
    }),
  });
  if (!res.ok) fail(`meeting creation failed: ${res.status} ${await res.text()}`);
  const meeting = (await res.json()).meeting;
  if (meeting.e2ee !== e2ee) fail(`created meeting has the wrong trust mode: ${JSON.stringify(meeting)}`);
  console.log(`meeting ${meeting.code} created (e2ee=${e2ee})`);
  return meeting.code;
}

// `extra` carries the #28 test knobs (`&e2eeDegrade=1`, `&e2eeTag=1`);
// the app parses them with plain substring checks, so order is free.
async function openPrejoin(page, code, name, extra = '') {
  await page.goto(`${BASE}/m/${code}?test=1${extra}`, { waitUntil: 'domcontentloaded' });
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

async function sendChat(page, body) {
  await page.type('[data-testid="chat-input"]', body);
  await page.click('[data-testid="chat-send"]');
}

const chatLogText = (page) =>
  page.$eval('[data-testid="chat-log"]', (el) => el.textContent ?? '');

// Inbound video ssrcs -> decoded frames. ssrcs are stable RTP
// identifiers, unlike the provisional track/stream wrappers Chrome
// hands out at ontrack time; framesDecoded only grows if the receiver
// transform actually decrypts what arrives (a dropped or garbled
// frame never reaches the decoder).
async function videoInboundSnapshot(page) {
  return page.evaluate(async () => {
    const stats = await window.__huddlePc.getStats();
    const live = {};
    stats.forEach((report) => {
      // SSRC 0 is the SFU's bandwidth-probe padding (str0m, libwebrtc
      // convention): Chrome reports it as a video inbound-rtp that never
      // carries a frame. It is not a media stream — leave it out.
      if (report.type === 'inbound-rtp' && report.kind === 'video' && report.ssrc !== 0) {
        live[report.ssrc] = report.framesDecoded ?? 0;
      }
    });
    return live;
  });
}

async function audioInboundSnapshot(page) {
  return page.evaluate(async () => {
    const stats = await window.__huddlePc.getStats();
    const live = {};
    stats.forEach((report) => {
      if (report.type === 'inbound-rtp' && report.kind === 'audio') {
        live[report.ssrc] = report.bytesReceived ?? 0;
      }
    });
    return live;
  });
}

const sumValues = (map) => Object.values(map).reduce((a, b) => a + b, 0);

// True once inbound media is demonstrably flowing (a couple of
// seconds of decoded video frames and received audio bytes).
async function mediaFlows(page) {
  const videoBefore = await videoInboundSnapshot(page);
  const audioBefore = await audioInboundSnapshot(page);
  await delay(2000);
  const videoAfter = await videoInboundSnapshot(page);
  const audioAfter = await audioInboundSnapshot(page);
  return (
    sumValues(videoAfter) - sumValues(videoBefore) > 5 &&
    sumValues(audioAfter) - sumValues(audioBefore) > 1000
  );
}

// The probe object only republishes when the client handles a server
// message, so while waiting on the counters we nudge it with a chat
// message (relayed to everyone, including the sender).
const waitTransformStats = (who, page, refresh, { requireDecrypted = true } = {}) =>
  waitFor(`${who}: transform counters in the probe`, async () => {
    const s = await e2eeStats(page);
    if (
      s && s.attached > 0 && s.encryptedFrames > 0 &&
      (!requireDecrypted || s.decryptedFrames > 0)
    ) {
      return s;
    }
    await refresh();
    return null;
  });

// A sender may switch to a new epoch before every receiver has imported
// the new key; frames dropped in that window are by design. Allow a small
// bounded number; a large value would point at a stuck key-distribution
// path, and genuine decrypt failures (known key, bad auth) always fail.
const PENDING_KEY_MISS_LIMIT = 8;

function assertHealthyTransform(who, stats) {
  if (stats.decryptFail !== 0) {
    fail(`${who}: transform reported decrypt failures: ${JSON.stringify(stats)}`);
  }
  if ((stats.pendingKeyMiss ?? 0) > PENDING_KEY_MISS_LIMIT) {
    fail(`${who}: too many frames dropped waiting for epoch keys: ${JSON.stringify(stats)}`);
  }
  if (stats.degraded !== false) {
    fail(`${who}: transform reported degraded in a healthy call: ${JSON.stringify(stats)}`);
  }
}

async function main() {
  console.log(`booting server (${SERVER})`);
  const srv = await bootServer();
  let browser;
  const contexts = [];
  let refreshN = 0;
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

    const closeContexts = async () => {
      for (const context of contexts.splice(0)) await context.close().catch(() => {});
    };

    // ---------------------------------------------------------------
    // Scenario 1: E2EE meeting — key exchange, verification, media
    // transform, chat round trip, late-joiner placeholder.
    // ---------------------------------------------------------------
    const code = await createMeeting(true);

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

    console.log('encrypted media still flows through the SFrame transforms');
    await waitFor('decoded inbound media on alice', () => mediaFlows(alice));
    await waitFor('decoded inbound media on bob', () => mediaFlows(bob));

    console.log('chat bodies round-trip encrypted but readable to key holders');
    const secret = 'the cake is a lie';
    await sendChat(alice, secret);
    await waitFor("alice's own chat log to show the message",
      async () => (await chatLogText(alice)).includes(secret));
    await waitFor("bob's chat log to show the message",
      async () => (await chatLogText(bob)).includes(secret));

    console.log('the probe reports live transform counters with zero decrypt failures');
    const refreshProbes = async () => {
      refreshN += 1;
      await sendChat(alice, `probe refresh ${refreshN}`);
    };
    const aliceTransform = await waitTransformStats('alice', alice, refreshProbes);
    const bobTransform = await waitTransformStats('bob', bob, refreshProbes);
    assertHealthyTransform('alice', aliceTransform);
    assertHealthyTransform('bob', bobTransform);
    console.log(`transforms healthy: ${JSON.stringify(aliceTransform)}`);

    console.log('a late joiner sees only placeholders for earlier chat (ciphertext persists)');
    const carol = await newClient('carol');
    await openPrejoin(carol, code, 'carol');
    await clickJoin(carol);
    await expectInCall(carol, 'carol');
    await waitFor("carol's replayed history to show the placeholder",
      async () => (await chatLogText(carol)).includes('Encrypted message'));
    await carol.waitForSelector('[data-testid="e2ee-indicator"]', { timeout: 30_000 })
      .catch(async () => fail('carol: the e2ee indicator never showed'));

    await closeContexts();

    // ---------------------------------------------------------------
    // Scenario 2: the degrade knob trips the EMA detector (D5).
    // ---------------------------------------------------------------
    const degradeCode = await createMeeting(true);
    const dAlice = await newClient('degrade-alice');
    const dBob = await newClient('degrade-bob');
    console.log('a forced-slow transform trips the degrade notice');
    // Two participants: the EMA only sees frames once the key exchange
    // delivers a sender key, and a solo caller never mints one.
    await openPrejoin(dAlice, degradeCode, 'alice', '&e2eeDegrade=1');
    await clickJoin(dAlice);
    await expectInCall(dAlice, 'alice');
    await openPrejoin(dBob, degradeCode, 'bob', '&e2eeDegrade=1');
    await clickJoin(dBob);
    await expectInCall(dBob, 'bob');
    await dAlice.waitForFunction(
      () => document.querySelector('[data-testid="call-status"]')?.textContent.includes('struggling'),
      { timeout: 30_000 },
    ).catch(async () => fail('the degrade notice never appeared'));
    const degradeStats = await waitFor('degraded flag in the probe', async () => {
      const s = await e2eeStats(dAlice);
      if (s && s.degraded === true && s.encryptedFrames > 0) return s;
      refreshN += 1;
      await sendChat(dAlice, `degrade refresh ${refreshN}`);
      return null;
    });
    if (degradeStats.emaMs <= 20) {
      fail(`degraded but emaMs is not over threshold: ${JSON.stringify(degradeStats)}`);
    }
    console.log(`degrade fired: ${JSON.stringify(degradeStats)}`);

    await closeContexts();

    // ---------------------------------------------------------------
    // Scenario 3: plaintext meeting in tag-only mode proves the whole
    // transform + SFU plumbing without WebCrypto (D10).
    // ---------------------------------------------------------------
    const tagCode = await createMeeting(false);
    const tAlice = await newClient('tag-alice');
    const tBob = await newClient('tag-bob');
    console.log('a plaintext meeting runs the transform plumbing in tag-only mode');
    await openPrejoin(tAlice, tagCode, 'alice', '&e2eeTag=1');
    await clickJoin(tAlice);
    await expectInCall(tAlice, 'alice');
    await openPrejoin(tBob, tagCode, 'bob', '&e2eeTag=1');
    await clickJoin(tBob);
    await expectInCall(tBob, 'bob');
    await tAlice.waitForSelector('[data-testid="roster-bob"]', { timeout: 30_000 })
      .catch(async () => fail('tag mode: alice never saw bob join'));
    if (await tAlice.$('[data-testid="e2ee-indicator"]')) {
      fail('the e2ee indicator showed in a plaintext meeting');
    }

    console.log('tagged media flows through the transforms and the SFU');
    await waitFor('decoded inbound media on tag-alice', () => mediaFlows(tAlice));
    await waitFor('decoded inbound media on tag-bob', () => mediaFlows(tBob));

    const refreshTagProbes = async () => {
      refreshN += 1;
      await sendChat(tAlice, `tag refresh ${refreshN}`);
    };
    const tAliceStats = await waitTransformStats('tag-alice', tAlice, refreshTagProbes);
    const tBobStats = await waitTransformStats('tag-bob', tBob, refreshTagProbes);
    assertHealthyTransform('tag-alice', tAliceStats);
    assertHealthyTransform('tag-bob', tBobStats);
    if (tAliceStats.enabled !== false) fail('probe says e2ee is on in a plaintext meeting');
    console.log(`tag-mode plumbing healthy: ${JSON.stringify(tAliceStats)}`);

    console.log('PASS: key exchange, verification, SFrame media, e2ee chat, degrade, tag-mode plumbing');
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
