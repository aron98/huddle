#!/usr/bin/env node
// N-party end-to-end call test with join/leave churn, screen share, and
// chat persistence.
//
// Boots a real huddle server, opens the meeting in headless Chrome
// pages with fake cameras/microphones, and asserts that each page
// receives the others' audio *and* video through the SFU (there is no
// peer-to-peer path at all), that a late joiner gets fresh media
// rather than frozen tiles, that a departing participant's tile and
// roster entry disappear everywhere, that mute state broadcasts
// propagate, that a shared screen reaches subscribers as a live,
// decoding tile (and withdraws when stopped), that a second share
// preempts the first everywhere, and that chat survives
// a participant reloading and rejoining.

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

// Like `waitFor`, but resolves `false` on timeout instead of failing.
// Used for the sender-liveness gates of the simulcast assertions: under
// runner load the sender's encoder can stall a layer, which makes the
// receiver-side upgrade unobservable no matter what the SFU does —
// that must degrade into a loud warning, not a test failure (issue #78).
async function tryWaitFor(what, fn, timeoutMs = 30_000, everyMs = 500) {
  const deadline = Date.now() + timeoutMs;
  let last;
  for (;;) {
    last = await fn();
    if (last) return last;
    if (Date.now() > deadline) {
      console.warn(`WARNING: timed out waiting for ${what}: ${JSON.stringify(last)}`);
      return false;
    }
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

async function joinAs(page, code, name, query = '?test=1') {
  await page.goto(`${BASE}/m/${code}${query}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-testid="name-input"]');
  await page.type('[data-testid="name-input"]', name);
  // Join is gated on the local stream being live (the button enables
  // once getUserMedia has produced tracks) — wait for that, not just
  // for the button to exist.
  await page.waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: media never became ready (status: ${await statusText(page)})`));
  // Click the button itself rather than screen coordinates: the join
  // click races the preview's first video frame, and a coordinate click
  // dispatched while that frame reflows the page can miss the button.
  // el.click() delivers a real (bubbling) click event to the element,
  // so handler regressions are still caught.
  await page.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
  // The prejoin form goes away once the call view mounts.
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
        `join as ${name} never left the prejoin page ` +
          `(status: ${await statusText(page)}, prejoin: ${JSON.stringify(stuck)})`,
      );
    });
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

// Snapshot of inbound video ssrcs → decoded frames. ssrcs are stable
// RTP identifiers, unlike the provisional track/stream wrapper objects
// Chrome hands out at ontrack time (same quirk class as the unstable
// MediaStream.id in the tile dedupe).
async function videoInboundSnapshot(page) {
  return page.evaluate(async () => {
    const stats = await window.__huddlePc.getStats();
    const live = {};
    stats.forEach((report) => {
      if (report.type === 'inbound-rtp' && report.kind === 'video') {
        live[report.ssrc] = report.framesDecoded ?? 0;
      }
    });
    return live;
  });
}

// Snapshot of inbound AUDIO ssrcs → received bytes. Share audio has no
// decodable-frame counter worth asserting here, so bytes are the
// liveness signal; ssrcs (not track ids) for the same reason as the
// video snapshot above.
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

// Per-ssrc video decode details (decoded frames + frame width). The
// SFU forwards over one ssrc per m-line and switches which simulcast
// rid it feeds, so a layer switch shows as a frameWidth CHANGE on the
// SAME ssrc (fresh decodable frames), never a new ssrc. Layer routing
// is asserted from ssrc snapshots, never from track ids (unstable
// provisional wrappers).
const videoDetails = (page) =>
  page.evaluate(async () => {
    const stats = await window.__huddlePc.getStats();
    const live = {};
    stats.forEach((report) => {
      if (report.type === 'inbound-rtp' && report.kind === 'video' && (report.framesDecoded ?? 0) > 0) {
        live[report.ssrc] = {
          framesDecoded: report.framesDecoded,
          frameWidth: report.frameWidth ?? 0,
        };
      }
    });
    return live;
  });

// Per-rid outbound simulcast stats of the SENDER (Chrome keys
// outbound-rtp per encoding rid): whether each layer is actually being
// produced. Under runner load the sender's encoder can stall a layer
// (issue #78), so the receiver-side layer checks are gated on this.
const senderSimulcast = (page) =>
  page.evaluate(async () => {
    const stats = await window.__huddlePc.getStats();
    const layers = {};
    stats.forEach((report) => {
      if (report.type === 'outbound-rtp' && report.kind === 'video' && report.rid) {
        layers[report.rid] = {
          framesSent: report.framesSent ?? 0,
          frameWidth: report.frameWidth ?? 0,
          active: report.active ?? null,
          reason: report.qualityLimitationReason ?? null,
        };
      }
    });
    return layers;
  });

// The simulcast layer the SFU is currently forwarding to `peer` from
// `origin`'s camera, straight from /metrics. This is the authoritative,
// load-independent proof that a layer preference landed server-side.
async function sfuSimulcastLayer(peer, origin) {
  try {
    const res = await fetch(`${BASE}/metrics`);
    const text = await res.text();
    const m = text.match(
      new RegExp(
        `^huddle_peer_simulcast_layer\\{[^}]*peer="${peer}"[^}]*origin="${origin}"[^}]*\\} (\\d+)$`,
        'm',
      ),
    );
    return m ? (m[1] === '1' ? 'high' : 'low') : null;
  } catch {
    return null;
  }
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

    console.log('asserting simulcast layers switch on tile focus (issue #12)');
    // Bob's only remote camera right now is alice's, so every decoding
    // video ssrc below belongs to her camera — no tile-to-ssrc mapping
    // needed. The SFU forwards one ssrc per m-line and switches which
    // rid it feeds, so the layer switch is a frameWidth change on the
    // SAME ssrc. Default subscription: the low layer (scale 4 on the
    // 640px fake camera → 160px wide).
    let preFocus = null;
    await waitFor('alice’s camera to start decoding at bob', async () => {
      preFocus = await videoDetails(bob);
      return Object.keys(preFocus).length > 0;
    }, 60_000).catch(async () => {
      await stateDump(bob, 'bob').catch(() => {});
      fail(`bob: no decoding video ssrcs before the focus test: ${JSON.stringify(preFocus)}`);
    });
    const ssrcs = Object.keys(preFocus);
    if (ssrcs.length !== 1) {
      fail(`bob: expected exactly one decoding camera ssrc, got ${JSON.stringify(preFocus)}`);
    }
    const camSsrc = ssrcs[0];
    const lowDecodedAt = preFocus[camSsrc].framesDecoded;
    if (preFocus[camSsrc].frameWidth > 320) {
      fail(
        `bob: expected the low layer (<=320px) before focus, ` +
          `ssrc ${camSsrc} is ${preFocus[camSsrc].frameWidth}px`,
      );
    }

    // Focusing alice's tile switches bob onto the high layer. The check
    // is three-legged so a loaded CI runner cannot flake it (issue #78):
    //   1. /metrics proves the SFU applied the preference — that is
    //      deterministic, independent of sender/receiver timing,
    //   2. the sender's high rid must keep producing frames — if the
    //      sender's encoder stalls under load, the receiver-side upgrade
    //      is unobservable regardless of the SFU, so the width check
    //      is skipped with a loud warning instead of failing,
    //   3. when the sender does produce, the receiver must genuinely
    //      decode the high layer: the SAME ssrc delivering bigger frames.
    await bob.click('[data-testid="tile-alice"]');
    await bob
      .waitForFunction(
        () => document.querySelector('[data-testid="tile-alice"]')?.classList.contains('focused'),
        { timeout: 10_000, polling: 250 },
      )
      .catch(() => fail('bob: focusing alice did not mark the tile focused'));
    await waitFor('the SFU to switch bob onto the high layer for alice', async () => {
      return (await sfuSimulcastLayer('bob', 'alice')) === 'high';
    }, 30_000).catch(async () => {
      await stateDump(bob, 'bob').catch(() => {});
      fail(
        `bob: the SFU never applied the high layer preference for alice ` +
          `(metrics layer: ${await sfuSimulcastLayer('bob', 'alice')})`,
      );
    });
    const highSentBefore = (await senderSimulcast(alice)).high?.framesSent ?? 0;
    let senderSnap = null;
    const highProducing = await tryWaitFor(
      "alice's high layer to keep producing frames",
      async () => {
        senderSnap = await senderSimulcast(alice);
        return (senderSnap.high?.framesSent ?? 0) > highSentBefore;
      },
      30_000,
    );
    if (!highProducing) {
      console.warn(
        `WARNING: alice's encoder is not producing the high layer ` +
          `(${JSON.stringify(senderSnap)}); skipping bob's receiver-side width check`,
      );
    } else {
      // The high layer genuinely arrives: same ssrc, bigger frames.
      let focusSnap = null;
      await waitFor('the high layer to arrive after focus', async () => {
        focusSnap = await videoDetails(bob);
        return (
          (focusSnap[camSsrc]?.framesDecoded ?? 0) > lowDecodedAt &&
          (focusSnap[camSsrc]?.frameWidth ?? 0) > 320
        );
      }, 60_000).catch(async () => {
        await stateDump(bob, 'bob').catch(() => {});
        fail(
          `bob: alice’s camera never switched to the high layer after focus ` +
            `(sender: ${JSON.stringify(senderSnap)}, pre: ${JSON.stringify(preFocus)}, ` +
            `post: ${JSON.stringify(focusSnap)})`,
        );
      });
      // Fresh decodable frames: the high layer keeps decoding.
      const highDecodedAt = focusSnap[camSsrc].framesDecoded;
      await delay(1000);
      const grownSnap = await videoDetails(bob);
      if ((grownSnap[camSsrc]?.framesDecoded ?? 0) <= highDecodedAt) {
        fail(`bob: the high layer on ssrc ${camSsrc} is not decoding fresh frames`);
      }
      console.log(
        `bob: focus switched alice to the high layer ` +
          `(ssrc ${camSsrc} now ${grownSnap[camSsrc].frameWidth}px, still decoding)`,
      );
    }

    // Unfocusing falls back to the low layer, with the same three-legged
    // structure: /metrics proves the SFU switched back, the sender's low
    // rid must keep producing, and the same ssrc then shrinks back to
    // <=320px and resumes decoding fresh frames.
    await bob.click('[data-testid="tile-alice"]');
    await waitFor('the SFU to switch bob back onto the low layer for alice', async () => {
      return (await sfuSimulcastLayer('bob', 'alice')) === 'low';
    }, 30_000).catch(async () => {
      await stateDump(bob, 'bob').catch(() => {});
      fail(
        `bob: the SFU never fell back to the low layer preference for alice ` +
          `(metrics layer: ${await sfuSimulcastLayer('bob', 'alice')})`,
      );
    });
    const lowSentBefore = (await senderSimulcast(alice)).low?.framesSent ?? 0;
    const lowProducing = await tryWaitFor(
      "alice's low layer to keep producing frames",
      async () => {
        senderSnap = await senderSimulcast(alice);
        return (senderSnap.low?.framesSent ?? 0) > lowSentBefore;
      },
      30_000,
    );
    if (!lowProducing) {
      console.warn(
        `WARNING: alice's encoder is not producing the low layer ` +
          `(${JSON.stringify(senderSnap)}); skipping bob's receiver-side width check`,
      );
    } else {
      const unfocusBaseline = (await videoDetails(bob))[camSsrc]?.framesDecoded ?? 0;
      await waitFor('the low layer to resume after unfocus', async () => {
        const snap = await videoDetails(bob);
        return (
          (snap[camSsrc]?.framesDecoded ?? 0) > unfocusBaseline &&
          (snap[camSsrc]?.frameWidth ?? 9999) <= 320
        );
      }, 60_000).catch(async () => {
        const snap = await videoDetails(bob);
        fail(
          `bob: the low layer never resumed after unfocus ` +
            `(ssrc ${camSsrc} decoded ${unfocusBaseline} at unfocus, ` +
            `now ${snap[camSsrc]?.framesDecoded ?? 'n/a'} @ ${snap[camSsrc]?.frameWidth ?? 'n/a'}px, ` +
            `sender: ${JSON.stringify(senderSnap)}, pre: ${JSON.stringify(preFocus)}, ` +
            `now: ${JSON.stringify(snap)})`,
        );
      });
      console.log('bob: unfocus fell back to the low layer (same ssrc, small frames resumed)');
    }

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
    // Leaving must land on the designed post-leave page (issue #5):
    // a rejoin button, the meeting info, and a way back to the dashboard.
    await dave.waitForFunction(
      () => !!document.querySelector('[data-testid="rejoin-btn"]'),
      { timeout: 10_000, polling: 250 },
    );
    const daveEndInfo = await dave.evaluate(
      () => document.querySelector('.end-info')?.textContent ?? '',
    );
    if (!daveEndInfo.includes(code)) {
      fail(`dave: post-leave view is missing meeting info (code ${code}): ${daveEndInfo}`);
    }
    if (!(await dave.evaluate(() => !!document.querySelector('a[href="/meetings"]')))) {
      fail('dave: post-leave view is missing the back-to-dashboard link');
    }
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

    console.log('asserting screen share reaches subscribers');
    // Which video ssrcs exist before the share starts.
    const videoBefore = await videoInboundSnapshot(bob);
    await alice.click('[data-testid="share-screen"]');
    // The start flow (getDisplayMedia → announce → renegotiate) finishes
    // when the button flips to its stop form.
    await alice
      .waitForFunction(
        () =>
          document.querySelector('[data-testid="share-screen"]')?.textContent.includes('Stop sharing'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => {
        await stateDump(alice, 'alice').catch(() => {});
        fail(`alice: screen share never started (status: ${await statusText(alice)})`);
      });
    await waitTile(bob, '[data-testid="tile-alice-screen"]', 'bob');
    await waitTile(carol, '[data-testid="tile-alice-screen"]', 'carol');
    await bob.waitForFunction(
      () => document.querySelector('[data-testid="roster-alice"]')?.textContent.includes('sharing'),
      { timeout: 30_000, polling: 250 },
    );
    // The screen must be live media: a NEW video ssrc (none of the
    // pre-share ones) that is actually decoding frames.
    let screenSnapshot = null;
    await waitFor(
      'bob receiving the shared screen',
      async () => {
        screenSnapshot = await videoInboundSnapshot(bob);
        const fresh = Object.keys(screenSnapshot).filter((s) => !(s in videoBefore));
        return fresh.length > 0 && fresh.every((s) => screenSnapshot[s] > 0);
      },
      60_000,
    ).catch(async () => {
      await stateDump(bob, 'bob').catch(() => {});
      fail(
        `bob: shared screen not decoding (before: ${JSON.stringify(videoBefore)}, ` +
          `after: ${JSON.stringify(screenSnapshot)})`,
      );
    });
    console.log('bob sees a fresh decoding video ssrc for the shared screen');

    console.log('alice stops sharing; the screen tile withdraws everywhere');
    await alice.click('[data-testid="share-screen"]');
    for (const [page, who] of [
      [bob, 'bob'],
      [carol, 'carol'],
    ]) {
      await page
        .waitForFunction(() => !document.querySelector('[data-testid="tile-alice-screen"]'), {
          timeout: 30_000,
          polling: 250,
        })
        .catch(async () => {
          await stateDump(page, who).catch(() => {});
          fail(`${who}: alice's screen tile never withdrew`);
        });
    }

    console.log('asserting a second share preempts the first (issue #6)');
    // Carol is the third page — a pure viewer for this section. Which
    // video ssrcs exist on her side before anyone shares again.
    const carolBeforeAliceShare = await videoInboundSnapshot(carol);
    await alice.click('[data-testid="share-screen"]');
    await alice
      .waitForFunction(
        () =>
          document.querySelector('[data-testid="share-screen"]')?.textContent.includes('Stop sharing'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => {
        await stateDump(alice, 'alice').catch(() => {});
        fail(`alice: second screen share never started (status: ${await statusText(alice)})`);
      });
    // The sharer sees their own share as a small picture-in-picture
    // preview with a stop control next to it (issue #6).
    await alice
      .waitForSelector('[data-testid="share-pip"]', { timeout: 10_000 })
      .catch(async () => {
        await stateDump(alice, 'alice').catch(() => {});
        fail('alice: share picture-in-picture preview never appeared');
      });
    await (async () => {
      try {
        await waitTile(carol, '[data-testid="tile-alice-screen"]', 'carol');
      } catch (e) {
        await stateDump(alice, 'alice').catch(() => {});
        await stateDump(bob, 'bob').catch(() => {});
        throw e;
      }
    })();
    // A screen on the grid switches the call to a stage layout: the
    // screen spans the stage and participants collapse into a strip.
    await carol
      .waitForFunction(
        () => document.querySelector('.tiles')?.classList.contains('has-screen'),
        { timeout: 10_000, polling: 250 },
      )
      .catch(async () => {
        await stateDump(carol, 'carol').catch(() => {});
        fail('carol: the tiles container never entered the stage layout');
      });
    // And the screen must be live, decoding media — ssrcs, not track ids.
    let aliceShareLive = null;
    await waitFor(
      "carol receiving alice's shared screen",
      async () => {
        aliceShareLive = await videoInboundSnapshot(carol);
        const fresh = Object.keys(aliceShareLive).filter((s) => !(s in carolBeforeAliceShare));
        return fresh.length > 0 && fresh.every((s) => aliceShareLive[s] > 0);
      },
      60_000,
    ).catch(async () => {
      await stateDump(carol, 'carol').catch(() => {});
      fail(
        `carol: alice's shared screen not decoding (before: ${JSON.stringify(carolBeforeAliceShare)}, ` +
          `after: ${JSON.stringify(aliceShareLive)})`,
      );
    });
    const aliceScreenSsrcs = Object.keys(aliceShareLive).filter(
      (s) => !(s in carolBeforeAliceShare),
    );
    console.log(`carol sees alice's screen live (fresh ssrcs: ${aliceScreenSsrcs.join(', ')})`);

    // Bob starts his own share: the server preempts alice's, so every
    // viewer sees only bob's screen and alice's share winds down.
    await bob.click('[data-testid="share-screen"]');
    await bob
      .waitForFunction(
        () =>
          document.querySelector('[data-testid="share-screen"]')?.textContent.includes('Stop sharing'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => {
        await stateDump(bob, 'bob').catch(() => {});
        fail(`bob: preempting screen share never started (status: ${await statusText(bob)})`);
      });
    await bob.waitForSelector('[data-testid="share-pip"]', { timeout: 10_000 }).catch(async () => {
      await stateDump(bob, 'bob').catch(() => {});
      fail('bob: share picture-in-picture preview never appeared');
    });
    await waitTile(carol, '[data-testid="tile-bob-screen"]', 'carol');
    await carol
      .waitForFunction(() => !document.querySelector('[data-testid="tile-alice-screen"]'), {
        timeout: 30_000,
        polling: 250,
      })
      .catch(async () => {
        await stateDump(carol, 'carol').catch(() => {});
        fail("carol: alice's screen tile never withdrew after bob preempted her");
      });
    // The roster hands over: bob sharing, alice not.
    await carol
      .waitForFunction(
        () => {
          const bobRoster = document.querySelector('[data-testid="roster-bob"]')?.textContent ?? '';
          const aliceRoster =
            document.querySelector('[data-testid="roster-alice"]')?.textContent ?? '';
          return bobRoster.includes('sharing') && !aliceRoster.includes('sharing');
        },
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => {
        await stateDump(carol, 'carol').catch(() => {});
        fail('carol: roster never handed the share from alice to bob');
      });
    // The share changed hands but never stopped, so the stage layout
    // must still be up.
    if (
      !(await carol.evaluate(
        () => document.querySelector('.tiles')?.classList.contains('has-screen') ?? false,
      ))
    ) {
      await stateDump(carol, 'carol').catch(() => {});
      fail('carol: stage layout dropped during the share hand-off');
    }
    // Bob's screen is a NEW decoding ssrc, distinct from everything
    // carol had before (cameras + alice's still-live screen).
    let bobShareLive = null;
    const priorToBob = { ...carolBeforeAliceShare, ...aliceShareLive };
    await waitFor(
      "carol receiving bob's shared screen",
      async () => {
        bobShareLive = await videoInboundSnapshot(carol);
        const fresh = Object.keys(bobShareLive).filter((s) => !(s in priorToBob));
        return fresh.length > 0 && fresh.every((s) => bobShareLive[s] > 0);
      },
      60_000,
    ).catch(async () => {
      await stateDump(carol, 'carol').catch(() => {});
      fail(
        `carol: bob's shared screen not decoding (before: ${JSON.stringify(carolBeforeAliceShare)}, ` +
          `alice live: ${JSON.stringify(aliceShareLive)}, after: ${JSON.stringify(bobShareLive)})`,
      );
    });
    // Alice's preempted screen must wind down on carol's side too: its
    // ssrcs stop decoding fresh frames (the reports can linger after
    // deactivation, so compare frame growth, not presence).
    const windDownStart = await videoInboundSnapshot(carol);
    await delay(1500);
    const windDownEnd = await videoInboundSnapshot(carol);
    for (const ssrc of aliceScreenSsrcs) {
      if ((windDownEnd[ssrc] ?? 0) > (windDownStart[ssrc] ?? 0)) {
        fail(
          `carol: alice's preempted screen ssrc ${ssrc} is still decoding fresh frames ` +
            `(${windDownStart[ssrc] ?? 0} -> ${windDownEnd[ssrc] ?? 0})`,
        );
      }
    }
    // The preempted sharer is stood down on her own page as well: the
    // button flips back, her preview disappears, and she becomes a
    // viewer of bob's screen.
    await alice
      .waitForFunction(
        () =>
          document.querySelector('[data-testid="share-screen"]')?.textContent.includes('Share screen'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => {
        await stateDump(alice, 'alice').catch(() => {});
        fail(`alice: preempted share never stood down (status: ${await statusText(alice)})`);
      });
    await alice
      .waitForFunction(() => !document.querySelector('[data-testid="share-pip"]'), {
        timeout: 30_000,
        polling: 250,
      })
      .catch(async () => {
        await stateDump(alice, 'alice').catch(() => {});
        fail("alice: share preview never disappeared after preemption");
      });
    await waitTile(alice, '[data-testid="tile-bob-screen"]', 'alice');
    console.log("bob preempted alice: carol and alice see only bob's screen");

    // Bob stops; the share fully withdraws and the layout reverts.
    await bob.click('[data-testid="share-screen"]');
    for (const [page, who] of [
      [alice, 'alice'],
      [carol, 'carol'],
    ]) {
      await page
        .waitForFunction(() => !document.querySelector('[data-testid="tile-bob-screen"]'), {
          timeout: 30_000,
          polling: 250,
        })
        .catch(async () => {
          await stateDump(page, who).catch(() => {});
          fail(`${who}: bob's screen tile never withdrew`);
        });
    }
    await carol
      .waitForFunction(() => !document.querySelector('.tiles')?.classList.contains('has-screen'), {
        timeout: 30_000,
        polling: 250,
      })
      .catch(async () => {
        await stateDump(carol, 'carol').catch(() => {});
        fail('carol: stage layout never reverted after bob stopped sharing');
      });
    await bob
      .waitForFunction(() => !document.querySelector('[data-testid="share-pip"]'), {
        timeout: 30_000,
        polling: 250,
      })
      .catch(async () => fail('bob: share preview never disappeared after stopping'));
    console.log("bob's share withdrew everywhere and the layout reverted");

    console.log('asserting a share can also carry the computer sound (issue #33)');
    const frank = await newClient('frank');
    // The screen-audio=1 knob keeps the fake capture stream's audio
    // track in the share (real picks keep whatever the browser's own
    // share-sound checkbox produced).
    await joinAs(frank, code, 'frank', '?test=1&screen-audio=1');

    const bobAudioBefore = await audioInboundSnapshot(bob);
    // Frank just joined: wait for the meeting footer to render before
    // clicking (page.click does not auto-wait for the selector).
    await frank.waitForSelector('[data-testid="share-screen"]', { timeout: 30_000 });
    await frank.click('[data-testid="share-screen"]');
    await frank
      .waitForFunction(
        () =>
          document
            .querySelector('[data-testid="share-screen"]')
            ?.textContent.includes('Stop sharing'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => fail(`frank: audio-bearing share never started (status: ${await statusText(frank)})`));

    // The sharer's own preview flags the sound and offers the
    // mid-share audio toggle.
    await frank
      .waitForSelector('[data-testid="share-audio-badge"]', { timeout: 10_000 })
      .catch(() => fail('frank: share-audio badge never appeared on the preview'));
    await frank
      .waitForSelector('[data-testid="share-audio-toggle"]', { timeout: 10_000 })
      .catch(() => fail('frank: share-audio toggle never appeared'));

    // Viewers get the screen tile WITH the audio badge, and a fresh
    // inbound AUDIO ssrc actually delivering bytes.
    await waitTile(bob, '[data-testid="tile-frank-screen"]', 'bob');
    await bob
      .waitForSelector('[data-testid="tile-frank-screen"] [data-testid="screen-audio-badge"]', {
        timeout: 10_000,
      })
      .catch(() => fail('bob: screen-audio badge missing from the share tile'));
    await waitFor(
      'bob receiving the share audio',
      async () => {
        const audio = await audioInboundSnapshot(bob);
        const fresh = Object.keys(audio).filter((s) => !(s in bobAudioBefore));
        return fresh.length > 0 && fresh.every((s) => audio[s] > 0);
      },
      30_000,
    );
    console.log('bob hears frank audio-bearing share (fresh audio ssrc with bytes)');

    // Flipping the sharer's audio toggle must not disturb the share:
    // the m-line stays up (a disabled track sends silence — no
    // renegotiation involved) and the video keeps decoding.
    const bobVideoAtFlip = await videoInboundSnapshot(bob);
    await frank.click('[data-testid="share-audio-toggle"]');
    await frank
      .waitForFunction(
        () =>
          document
            .querySelector('[data-testid="share-audio-toggle"]')
            ?.textContent.includes('audio off'),
        { timeout: 10_000, polling: 250 },
      )
      .catch(() => fail('frank: audio toggle never flipped to off'));
    await waitFor(
      "the share video to keep decoding after the audio flip",
      async () => {
        const video = await videoInboundSnapshot(bob);
        return Object.keys(bobVideoAtFlip).some((s) => (video[s] ?? 0) > bobVideoAtFlip[s]);
      },
      15_000,
    );

    // Frank stops; tile and badge withdraw.
    await frank.click('[data-testid="share-screen"]');
    await bob
      .waitForFunction(() => !document.querySelector('[data-testid="tile-frank-screen"]'), {
        timeout: 30_000,
        polling: 250,
      })
      .catch(() => fail("bob: frank's audio-bearing share tile never withdrew"));

    // A re-share re-delivers the sound under the same state-preservation
    // rules as the video.
    const bobAudioBefore2 = await audioInboundSnapshot(bob);
    await frank.click('[data-testid="share-screen"]');
    await frank
      .waitForFunction(
        () =>
          document
            .querySelector('[data-testid="share-screen"]')
            ?.textContent.includes('Stop sharing'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => fail(`frank: re-share never started (status: ${await statusText(frank)})`));
    await waitTile(bob, '[data-testid="tile-frank-screen"]', 'bob');
    await waitFor(
      'bob receiving the re-shared audio',
      async () => {
        const audio = await audioInboundSnapshot(bob);
        const fresh = Object.keys(audio).filter((s) => !(s in bobAudioBefore2));
        return fresh.length > 0 && fresh.every((s) => audio[s] > 0);
      },
      30_000,
    );
    console.log("frank's audio-bearing share flowed, toggled, withdrew, and re-shared cleanly");

    console.log('asserting chat and its persistence across a rejoin');
    await alice.type('[data-testid="chat-input"]', 'hello from alice');
    await alice.click('[data-testid="chat-send"]');
    await bob
      .waitForFunction(
        () =>
          [...document.querySelectorAll('[data-testid="chat-log"] li')].some((li) =>
            li.textContent.includes('hello from alice'),
          ),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => {
        await stateDump(bob, 'bob').catch(() => {});
        fail('bob never saw the chat message');
      });
    // Reload bob: the server replays persisted history to rejoiners.
    await joinAs(bob, code, 'bob');
    await bob
      .waitForFunction(
        () =>
          [...document.querySelectorAll('[data-testid="chat-log"] li')].some((li) =>
            li.textContent.includes('hello from alice'),
          ),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => {
        await stateDump(bob, 'bob').catch(() => {});
        fail('chat history did not survive bob reloading and rejoining');
      });
    console.log('bob rejoined and sees the persisted chat history');

    console.log('asserting prejoin toggles hit only their own track kind (issue #7)');
    const erin = await newClient('erin');
    await erin.goto(`${BASE}/m/${code}?test=1`, { waitUntil: 'domcontentloaded' });
    await erin
      .waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
      .catch(async () => fail(`erin: media never became ready (status: ${await statusText(erin)})`));

    // track.enabled probe against the preview's live stream.
    const prejoinProbe = () =>
      erin.evaluate(() => {
        const stream = document.querySelector('#preview')?.srcObject;
        const audio = stream?.getAudioTracks()[0] ?? null;
        const video = stream?.getVideoTracks()[0] ?? null;
        return {
          hasStream: !!stream,
          audioEnabled: audio?.enabled ?? null,
          videoEnabled: video?.enabled ?? null,
          audioId: audio?.id ?? null,
          videoId: video?.id ?? null,
        };
      });

    let probe = await prejoinProbe();
    if (!probe.hasStream || probe.audioEnabled !== true || probe.videoEnabled !== true)
      fail(`erin: fresh prejoin stream not fully enabled: ${JSON.stringify(probe)}`);

    // Mic off must leave the camera track alone — before the fix, both
    // toggles hit every track in the stream and the preview went black.
    await erin.evaluate(() => document.querySelector('[data-testid="prejoin-toggle-mic"]')?.click());
    await waitFor(
      'mic toggle to disable only the audio track',
      async () => {
        probe = await prejoinProbe();
        return probe.audioEnabled === false && probe.videoEnabled === true;
      },
      10_000,
    ).catch(async () => fail(`erin: mic off also hit the video track: ${JSON.stringify(probe)}`));

    // And the converse: camera off must leave the (already muted) audio
    // track alone.
    await erin.evaluate(() => document.querySelector('[data-testid="prejoin-toggle-cam"]')?.click());
    await waitFor(
      'camera toggle to disable only the video track',
      async () => {
        probe = await prejoinProbe();
        return probe.videoEnabled === false && probe.audioEnabled === false;
      },
      10_000,
    ).catch(async () => fail(`erin: camera off also hit the audio track: ${JSON.stringify(probe)}`));

    // Camera back on: the muted mic must not come back with it.
    await erin.evaluate(() => document.querySelector('[data-testid="prejoin-toggle-cam"]')?.click());
    await waitFor(
      'camera re-enable to leave the mic muted',
      async () => {
        probe = await prejoinProbe();
        return probe.videoEnabled === true && probe.audioEnabled === false;
      },
      10_000,
    ).catch(async () => fail(`erin: camera re-enable resurrected the muted mic: ${JSON.stringify(probe)}`));
    console.log(`erin: prejoin toggles are kind-filtered (${JSON.stringify(probe)})`);

    // Device re-acquisition: picking a concrete mic re-runs getUserMedia,
    // and the fresh tracks come back enabled by default — the toggle
    // state must be re-applied (muted mic stays muted, camera stays on).
    const micDevice = await waitFor(
      'a concrete mic device in the prejoin picker',
      async () => {
        const opts = await erin.$$eval(
          '[data-testid="mic-select"] option',
          (os) => os.map((o) => o.value),
        );
        return opts.find((v) => v !== '') ?? false;
      },
      30_000,
    );
    const beforeReswitch = await prejoinProbe();
    await erin.select('[data-testid="mic-select"]', micDevice);
    await waitFor(
      're-acquired mic tracks to inherit the toggle state',
      async () => {
        probe = await prejoinProbe();
        return (
          probe.hasStream &&
          probe.audioId !== null &&
          probe.audioId !== beforeReswitch.audioId &&
          probe.audioEnabled === false &&
          probe.videoEnabled === true
        );
      },
      30_000,
    ).catch(async () =>
      fail(
        `erin: device re-acquisition resurrected the muted mic or disabled the camera: ` +
          `before ${JSON.stringify(beforeReswitch)}, after ${JSON.stringify(probe)}`,
      ),
    );
    console.log(`erin: device re-acquisition kept the toggle state (${JSON.stringify(probe)})`);

    // Join with the mic muted: the state must survive the transition —
    // the in-call buttons read track.enabled, and the roster (via the
    // server) must agree.
    await erin.type('[data-testid="name-input"]', 'erin');
    await erin.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
    await erin
      .waitForSelector('[data-testid="name-input"]', { hidden: true, timeout: 30_000 })
      .catch(async () => fail(`erin never left the prejoin page (status: ${await statusText(erin)})`));
    await erin
      .waitForFunction(
        () => document.querySelector('[data-testid="toggle-mic"]')?.textContent.includes('Mic off'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () =>
        fail(`erin: in-call mic button lost the prejoin mute (status: ${await statusText(erin)})`),
      );
    await erin
      .waitForFunction(
        () => document.querySelector('[data-testid="toggle-cam"]')?.textContent.includes('Camera on'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => fail('erin: in-call camera button lost the prejoin state'));
    await bob
      .waitForFunction(
        () => {
          const erinRoster = document.querySelector('[data-testid="roster-erin"]')?.textContent ?? '';
          return erinRoster.includes('mic off') && erinRoster.includes('cam on');
        },
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () => {
        await stateDump(bob, 'bob').catch(() => {});
        fail(`bob never saw erin's prejoin mute in the roster (status: ${await statusText(bob)})`);
      });
    console.log("erin's prejoin mute survived into the call (buttons and roster agree)");

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
