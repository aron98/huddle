#!/usr/bin/env node
// TURN-relayed call end-to-end test.
//
// Boots a local coturn instance plus a huddle server that advertises it
// as its ICE server, then joins a two-person call from headless Chrome
// pages loaded with `?turn=relay` — which forces every ICE candidate
// through the TURN relay (`iceTransportPolicy: "relay"`). The test
// passes only when both pages receive audio and video *and* the
// selected candidate pair on each side is a relay allocation, proving
// the whole chain (browser → coturn → SFU) works without any direct
// host-to-host path.
//
// coturn is spawned on port 3479 (not the default 3478) so a
// distribution's system turnserver — if one is running — never
// conflicts with the test instance.

import { spawn } from 'node:child_process';
import { createConnection } from 'node:net';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

const PORT = 3133;
const TURN_PORT = 3479;
const BASE = `http://127.0.0.1:${PORT}`;
const CHROME = process.env.CHROME_BIN ?? 'google-chrome';
const ROOT = resolve(import.meta.dirname, '../..');
const SERVER = join(ROOT, 'target/debug/huddle');
const TURN_USER = 'huddle';
const TURN_PASS = 'devpassword';

// Fail fast, but unwind through `finally` so everything cleans up.
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

// coturn listens on TCP too, which makes a cheap readiness probe.
function turnReady() {
  return new Promise((resolveProbe) => {
    const socket = createConnection({ host: '127.0.0.1', port: TURN_PORT });
    socket.once('connect', () => {
      socket.destroy();
      resolveProbe(true);
    });
    socket.once('error', () => resolveProbe(false));
  });
}

function bootTurn() {
  const dir = mkdtempSync(join(tmpdir(), 'huddle-turn-'));
  // A config file rather than CLI flags: coturn's CLI `--user` path
  // behaves differently across versions, and this is the exact shape
  // the README documents for self-hosters. `allow-loopback-peers` is
  // required because both the browsers and the SFU run on 127.0.0.1
  // in this test — in production coturn refuses to relay to loopback
  // (it would defeat the point of TURN).
  writeFileSync(
    join(dir, 'turnserver.conf'),
    [
      `listening-port=${TURN_PORT}`,
      'listening-ip=127.0.0.1',
      'relay-ip=127.0.0.1',
      'min-port=49160',
      'max-port=49165',
      'lt-cred-mech',
      `user=${TURN_USER}:${TURN_PASS}`,
      'realm=huddle.local',
      'allow-loopback-peers',
      'no-tls',
      'no-dtls',
      'no-cli',
      'pidfile=',
      '',
    ].join('\n'),
  );
  const turn = spawn('turnserver', ['-c', join(dir, 'turnserver.conf')], {
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let logs = '';
  turn.stdout.on('data', (d) => (logs += d));
  turn.stderr.on('data', (d) => (logs += d));
  turn.on('exit', (code) => (logs += `\n[turnserver exited: ${code}]`));
  return {
    turn,
    logs: () => logs,
    cleanup: () => {
      turn.kill('SIGTERM');
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

async function bootServer() {
  const dataDir = mkdtempSync(join(tmpdir(), 'huddle-turn-e2e-'));
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
      // The TURN server the server advertises in every Welcome. figment
      // parses array-of-dict *values* (TOML-ish `key=value` syntax,
      // colons do NOT work), unlike `__0__`-style key nesting, which
      // produces dicts that never reach a Vec field.
      HUDDLE_ICE_SERVERS: `[{urls=["turn:127.0.0.1:${TURN_PORT}"],username="${TURN_USER}",credential="${TURN_PASS}"}]`,
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
      title: 'e2e turn call',
      starts_at: rfc3339(new Date()),
      ends_at: rfc3339(new Date(Date.now() + 60 * 60 * 1000)),
    }),
  });
  if (!res.ok) fail(`meeting creation failed: ${res.status} ${await res.text()}`);
  const body = await res.json();
  return body.meeting.code;
}

async function joinAs(page, code, name) {
  // `turn=relay` is the knob that forces every candidate through the
  // TURN relay; without it Chrome would happily use the direct host
  // path and the relay assertion below would be vacuous.
  await page.goto(`${BASE}/m/${code}?test=1&turn=relay`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-testid="name-input"]');
  await page.type('[data-testid="name-input"]', name);
  await page.waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: media never became ready (status: ${await statusText(page)})`));
  await page.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
  await page
    .waitForSelector('[data-testid="name-input"]', { hidden: true, timeout: 60_000 })
    .catch(async () => {
      fail(`join as ${name} never left the prejoin page (status: ${await statusText(page)})`);
    });
}

// Inbound media through the SFU, same probe shape as e2e-call.mjs.
async function mediaProbe(page, peer) {
  return page.evaluate(async (peerName) => {
    const tile = document.querySelector(`[data-testid="tile-${peerName}"] video`);
    const pc = window.__huddlePc;
    if (!tile || !pc) return { tile: false };
    const stats = await pc.getStats();
    let audioBytes = 0;
    let videoBytes = 0;
    stats.forEach((report) => {
      if (report.type === 'inbound-rtp') {
        if (report.kind === 'audio') audioBytes += report.bytesReceived ?? 0;
        if (report.kind === 'video') videoBytes += report.bytesReceived ?? 0;
      }
    });
    return {
      tile: true,
      audioBytes,
      videoBytes,
      connection: pc.connectionState,
    };
  }, peer);
}

// Candidate-pair report: which local candidate type the selected pair
// uses. With iceTransportPolicy=relay, 'relay' here means the media
// path is browser → coturn allocation → SFU.
async function relayProbe(page) {
  return page.evaluate(async () => {
    const pc = window.__huddlePc;
    if (!pc) return { pc: false };
    const stats = await pc.getStats();
    const locals = new Map();
    stats.forEach((r) => {
      if (r.type === 'local-candidate') locals.set(r.id, r.candidateType);
    });
    const pairs = [];
    stats.forEach((r) => {
      if (r.type === 'candidate-pair') {
        pairs.push({
          state: r.state,
          selected: !!r.selected,
          localType: locals.get(r.localCandidateId) ?? null,
        });
      }
    });
    return { pc: true, pairs };
  });
}

async function main() {
  const turn = bootTurn();
  let server;
  let browser;
  try {
    await waitFor('turnserver to listen', turnReady, 15_000);
    console.log(`turnserver listening on 127.0.0.1:${TURN_PORT}`);

    server = await bootServer();
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

    console.log('alice and bob join through the relay');
    await joinAs(alice, code, 'alice');
    await joinAs(bob, code, 'bob');

    console.log('waiting for tiles');
    const waitTile = async (page, sel, who) => {
      await page.waitForSelector(sel, { timeout: 60_000 }).catch(async () => {
        const dump = await page
          .evaluate(() => {
            const pc = window.__huddlePc;
            return {
              status: document.querySelector('[data-testid="call-status"]')?.textContent ?? null,
              tiles: [...document.querySelectorAll('[data-testid^="tile-"]')].map((e) => e.dataset.testid),
              roster: [...document.querySelectorAll('[data-testid^="roster-"]')].map((e) => e.textContent),
              pc: pc
                ? {
                    conn: pc.connectionState,
                    ice: pc.iceConnectionState,
                    sig: pc.signalingState,
                    localM: pc.localDescription?.sdp.split('\n').filter((l) => l.startsWith('m=')).length,
                    remoteM: pc.remoteDescription?.sdp.split('\n').filter((l) => l.startsWith('m=')).length,
                  }
                : null,
            };
          })
          .catch(() => null);
        fail(`${who}: tile wait ${sel} failed; state: ${JSON.stringify(dump)}`);
      });
    };
    await waitTile(alice, '[data-testid="tile-bob"]', 'alice');
    await waitTile(bob, '[data-testid="tile-alice"]', 'bob');

    console.log('asserting media flows through the relay');
    const waitMedia = async (page, peer, who) => {
      let probe = null;
      await waitFor(
        `${who} receiving ${peer}'s media`,
        async () => {
          probe = await mediaProbe(page, peer);
          return probe.audioBytes > 0 && probe.videoBytes > 0;
        },
        90_000,
      ).catch(async () => {
        fail(`${who}: no media arriving from ${peer} via TURN: ${JSON.stringify(probe)}`);
      });
      console.log(`${who} <- ${peer} via TURN: ${JSON.stringify(probe)}`);
    };
    await waitMedia(alice, 'bob', 'alice');
    await waitMedia(bob, 'alice', 'bob');

    console.log('asserting the selected candidate pairs are TURN relay allocations');
    const waitRelayPair = async (page, who) => {
      let probe = null;
      await waitFor(
        `${who} selected a relay pair`,
        async () => {
          probe = await relayProbe(page);
          return probe.pairs?.some((p) => p.state === 'succeeded' && p.localType === 'relay');
        },
        30_000,
      ).catch(async () => {
        fail(`${who}: no succeeded relay candidate pair: ${JSON.stringify(probe)}`);
      });
      console.log(`${who} candidate pairs: ${JSON.stringify(probe.pairs)}`);
    };
    await waitRelayPair(alice, 'alice');
    await waitRelayPair(bob, 'bob');

    console.log('PASS: two-person call flowed through coturn (relay candidates selected)');
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (server) {
      server.cleanup();
      if (process.env.DUMP_SERVER_LOGS !== '0') console.log('--- server logs ---\n', server.logs());
    }
    turn.cleanup();
    if (process.env.DUMP_SERVER_LOGS !== '0') console.log('--- turn logs ---\n', turn.logs());
  }
}

main().catch((e) => {
  if (!(e instanceof FailSignal)) {
    console.error('FAIL: unexpected error', e);
    process.exitCode = 1;
  }
});
