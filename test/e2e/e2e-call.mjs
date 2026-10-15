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
// decoding tile (and withdraws when stopped), and that chat survives
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
