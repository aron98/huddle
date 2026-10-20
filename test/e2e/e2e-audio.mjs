#!/usr/bin/env node
// Audio settings end-to-end test (issue #34): explicit AEC constraints
// on every microphone acquisition, prejoin + in-call toggles,
// localStorage persistence, and an in-call swap that keeps media
// flowing.
//
// Boots a real huddle server and drives the real prejoin page and call
// view in headless Chrome (fake media devices):
//
//   1. a fresh prejoin track reports echoCancellation / noiseSuppression
//      / autoGainControl all true via getSettings();
//   2. toggling echo cancellation in the prejoin page re-acquires the
//      mic immediately (new track id, echoCancellation false, other
//      settings untouched) and persists the choice to localStorage;
//   3. a mic device switch re-acquires with the SAME audio settings
//      (constraints survive device switches);
//   4. reloading the prejoin page keeps the persisted settings;
//   5. in a call, toggling echo cancellation swaps the live track via
//      replaceTrack: the sender's track reports the new constraints,
//      and a second participant keeps receiving audio bytes.

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

const PORT = 3134;
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

async function createMeeting() {
  const rfc3339 = (date) => date.toISOString().replace(/\.\d+Z$/, 'Z');
  const res = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: 'e2e audio settings',
      starts_at: rfc3339(new Date()),
      ends_at: rfc3339(new Date(Date.now() + 60 * 60 * 1000)),
    }),
  });
  if (!res.ok) fail(`meeting creation failed: ${res.status} ${await res.text()}`);
  const body = await res.json();
  return body.meeting.code;
}

// Open the prejoin page as far as a clickable join button, optionally
// filling in the name so a later clickJoin enters the call.
async function openPrejoin(page, code, name) {
  await page.goto(`${BASE}/m/${code}?test=1`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-testid="name-input"]');
  if (name) await page.type('[data-testid="name-input"]', name);
  await page.waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
    .catch(async () => fail(`join as ${name ?? 'guest'}: media never became ready`));
}

async function joinAs(page, code, name) {
  await openPrejoin(page, code, name);
  // Click the element itself, not screen coordinates: a coordinate click
  // races the preview's first-frame reflow.
  await page.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
  await page
    .waitForSelector('[data-testid="name-input"]', { hidden: true, timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: never left the prejoin page`));
  await page
    .waitForSelector(`[data-testid="roster-${name}"]`, { timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: never made it into the call`));
}

// The prejoin page's live audio track, with its id and applied
// constraints (getSettings on Chrome's fake devices reports the
// constraints getUserMedia honoured).
const prejoinAudio = (page) =>
  page.evaluate(() => {
    const stream = document.querySelector('#preview')?.srcObject;
    const audio = stream?.getAudioTracks()[0] ?? null;
    if (!audio) return { audioId: null };
    const s = audio.getSettings();
    return {
      audioId: audio.id,
      echoCancellation: s.echoCancellation ?? null,
      noiseSuppression: s.noiseSuppression ?? null,
      autoGainControl: s.autoGainControl ?? null,
    };
  });

// The local sender's audio track inside the call, with its constraints.
const senderAudio = (page) =>
  page.evaluate(async () => {
    const pc = window.__huddlePc;
    if (!pc) return { audioId: null };
    for (const sender of pc.getSenders()) {
      const track = sender.track;
      if (track && track.kind === 'audio') {
        const s = track.getSettings();
        return {
          audioId: track.id,
          echoCancellation: s.echoCancellation ?? null,
          noiseSuppression: s.noiseSuppression ?? null,
          autoGainControl: s.autoGainControl ?? null,
        };
      }
    }
    return { audioId: null };
  });

// Total inbound audio bytes received through the SFU.
const inboundAudioBytes = async (page) => {
  const bytes = await page.evaluate(async () => {
    const pc = window.__huddlePc;
    if (!pc) return -1;
    const stats = await pc.getStats();
    let total = 0;
    stats.forEach((report) => {
      if (report.type === 'inbound-rtp' && report.kind === 'audio') {
        total += report.bytesReceived ?? 0;
      }
    });
    return total;
  });
  return bytes;
};

const clickToggle = (page, testid) =>
  page.evaluate((id) => document.querySelector(`[data-testid="${id}"]`)?.click(), testid);

async function main() {
  console.log(`booting server (${SERVER})`);
  const srv = await bootServer();
  let browser;
  try {
    const code = await createMeeting();
    console.log(`meeting ${code} created`);

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
    await openPrejoin(alice, code, null);

    console.log('a fresh prejoin track carries the explicit AEC constraints');
    let probe = await waitFor('prejoin audio track', () => prejoinAudio(alice), 30_000);
    if (probe.audioId === null) fail('alice: no audio track in the prejoin preview');
    if (probe.echoCancellation !== true || probe.noiseSuppression !== true ||
        probe.autoGainControl !== true) {
      fail(`alice: fresh track missing explicit constraints: ${JSON.stringify(probe)}`);
    }
    console.log(`alice: fresh settings ${JSON.stringify(probe)}`);

    console.log('toggling echo cancellation re-acquires the mic immediately');
    const beforeToggle = probe.audioId;
    await clickToggle(alice, 'toggle-echo-cancellation');
    probe = await waitFor(
      're-acquired track without echo cancellation',
      async () => {
        const fresh = await prejoinAudio(alice);
        return fresh.audioId !== null && fresh.audioId !== beforeToggle &&
          fresh.echoCancellation === false && fresh.noiseSuppression === true
          ? fresh
          : null;
      },
      30_000,
    ).catch(async () => {
      const fresh = await prejoinAudio(alice);
      fail(`alice: echo toggle did not re-acquire with new constraints: ${JSON.stringify(fresh)}`);
    });
    console.log(`alice: echo off, new track ${probe.audioId}`);

    console.log('the choice persists in localStorage');
    const stored = await alice.evaluate(() => localStorage.getItem('huddle.audio-settings'));
    let parsed = null;
    try {
      parsed = JSON.parse(stored);
    } catch {
      // handled below
    }
    if (!parsed || parsed.echo_cancellation !== false) {
      fail(`alice: localStorage did not persist the choice: ${stored}`);
    }

    console.log('a mic device switch keeps the chosen constraints');
    const micDevice = await waitFor(
      'a concrete mic device in the prejoin picker',
      async () => {
        const opts = await alice.$$eval(
          '[data-testid="mic-select"] option',
          (os) => os.map((o) => o.value),
        );
        return opts.find((v) => v !== '') ?? false;
      },
      30_000,
    );
    const beforeSwitch = (await prejoinAudio(alice)).audioId;
    await alice.select('[data-testid="mic-select"]', micDevice);
    probe = await waitFor(
      'device re-acquisition to keep echo cancellation off',
      async () => {
        const fresh = await prejoinAudio(alice);
        return fresh.audioId !== null && fresh.audioId !== beforeSwitch &&
          fresh.echoCancellation === false
          ? fresh
          : null;
      },
      30_000,
    ).catch(async () => {
      const fresh = await prejoinAudio(alice);
      fail(`alice: device switch dropped the audio settings: ${JSON.stringify(fresh)}`);
    });
    console.log(`alice: settings survived the device switch ${JSON.stringify(probe)}`);

    console.log('reloading the prejoin page restores the persisted settings');
    await openPrejoin(alice, code, 'alice');
    probe = await waitFor(
      'reloaded prejoin to keep echo cancellation off',
      () => prejoinAudio(alice).then((p) => (p.audioId !== null && p.echoCancellation === false ? p : null)),
      30_000,
    ).catch(async () => {
      const fresh = await prejoinAudio(alice);
      fail(`alice: reload lost the persisted settings: ${JSON.stringify(fresh)}`);
    });
    console.log('alice: persisted settings apply after reload');

    console.log('joining the call to toggle live');
    await alice.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
    await alice
      .waitForSelector('[data-testid="name-input"]', { hidden: true, timeout: 30_000 })
      .catch(async () => fail('alice: never left the prejoin page'));
    await alice
      .waitForSelector('[data-testid="roster-alice"]', { timeout: 30_000 })
      .catch(async () => fail('alice: never made it into the call'));

    const bob = await newClient('bob');
    await joinAs(bob, code, 'bob');
    await alice
      .waitForSelector('[data-testid="roster-bob"]', { timeout: 30_000 })
      .catch(async () => fail('alice: bob never appeared in the roster'));

    console.log('bob receives alice\u2019s audio through the SFU');
    const baseline = await waitFor(
      'bob to receive inbound audio bytes',
      async () => {
        const bytes = await inboundAudioBytes(bob);
        return bytes > 0 ? bytes : null;
      },
      30_000,
    );
    console.log(`bob: baseline inbound audio bytes ${baseline}`);

    console.log('alice re-enables echo cancellation in the call');
    const preSwap = await senderAudio(alice);
    await clickToggle(alice, 'toggle-echo-cancellation');
    await waitFor(
      'the re-acquisition status note',
      async () => {
        const status = await alice.evaluate(
          () => document.querySelector('[data-testid="call-status"]')?.textContent ?? null,
        );
        return status && /re-acquired/.test(status) ? status : null;
      },
      30_000,
    ).catch(async () => {
      const status = await alice.evaluate(
        () => document.querySelector('[data-testid="call-status"]')?.textContent ?? null,
      );
      fail(`alice: no re-acquisition status appeared (status: ${status})`);
    });

    console.log('the live sender reports a fresh track with echo cancellation on');
    const sender = await waitFor(
      'sender audio track with echo cancellation back on',
      async () => {
        const fresh = await senderAudio(alice);
        return fresh.audioId !== null && fresh.echoCancellation === true ? fresh : null;
      },
      30_000,
    ).catch(async () => {
      const fresh = await senderAudio(alice);
      fail(`alice: sender track does not report the new constraints: ${JSON.stringify(fresh)}`);
    });
    console.log(`alice: sender settings ${JSON.stringify(sender)}`);
    if (sender.audioId === preSwap.audioId) {
      fail(
        `alice: toggling did not swap the sender track (still ${sender.audioId})`,
      );
    }

    console.log('media keeps flowing to bob after the swap');
    await waitFor(
      'bob\u2019s inbound audio to keep growing',
      async () => {
        const bytes = await inboundAudioBytes(bob);
        return bytes > baseline ? bytes : null;
      },
      30_000,
    ).catch(async () => {
      const bytes = await inboundAudioBytes(bob);
      fail(`alice\u2019s track swap stopped media flow (bob at ${bytes} bytes, baseline ${baseline})`);
    });
    console.log('bob: audio kept flowing after the swap');

    console.log('PASS: explicit AEC constraints, toggles, persistence, and live swap all behave');
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
