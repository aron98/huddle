#!/usr/bin/env node
// Background blur e2e (issue #32): pipeline activation, blur carrying
// into the call, replaceTrack swaps that never renegotiate, preference
// persistence across a prejoin reload, and the degrade paths.
//
// Boots a real huddle server and opens headless Chrome pages with fake
// cameras. Asserts through ssrc snapshots (track ids are unstable
// provisional wrappers) that toggling blur swaps the local video track
// on the live sender WITHOUT touching the SDP: the receiving side keeps
// decoding on the same ssrcs, no renegotiation happens. The e2e-only
// knobs (?test=1&blur-fail=1 / &blur-slow=1) drive the two documented
// degrade paths (model failure / sustained slow frames) deterministically.

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
      title: 'e2e blur',
      starts_at: rfc3339(new Date()),
      ends_at: rfc3339(new Date(Date.now() + 60 * 60 * 1000)),
    }),
  });
  if (!res.ok) fail(`meeting creation failed: ${res.status} ${await res.text()}`);
  const body = await res.json();
  return body.meeting.code;
}

async function joinAs(page, code, name, extraQuery = '') {
  await page.goto(`${BASE}/m/${code}?test=1${extraQuery}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('[data-testid="name-input"]');
  await page.type('[data-testid="name-input"]', name);
  await page.waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
    .catch(async () => fail(`join as ${name}: media never became ready (status: ${await statusText(page)})`));
  await page.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
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

// Snapshot of inbound video ssrcs → decoded frames. ssrcs are stable RTP
// identifiers (see e2e-call.mjs for the track-id instability story);
// blur toggles must NEVER change them, only replaceTrack the source.
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

// The blur pipeline's e2e stats object (created by the web client under
// ?test=1): active/notice/error are owned by Rust, framesIn/framesOut/
// avgMs/degraded by the JS shim.
const blurStats = (page) => page.evaluate(() => window.__huddleEffectsStats ?? null);

// The local tile's current video track id — it must CHANGE when the
// pipeline swaps tracks, proving the in-place swap reached the UI.
const localVideoTrackId = (page) =>
  page.evaluate(() => document.querySelector('[data-testid="tile-you"] video')?.srcObject?.getVideoTracks()[0]?.id ?? null);

const stateDump = (page, who) =>
  page
    .evaluate(() => {
      const pc = window.__huddlePc;
      return {
        status: document.querySelector('[data-testid="call-status"]')?.textContent ?? null,
        tiles: [...document.querySelectorAll('[data-testid^="tile-"]')].map((e) => e.dataset.testid),
        blur: window.__huddleEffectsStats ?? null,
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

    // Scenarios that depend on a clean blur preference (degrade paths)
    // run in isolated incognito contexts: localStorage is otherwise
    // shared by every page in the browser, and earlier steps persist
    // their choice (the persistence assertion relies on that sharing).
    const isolatedContexts = [];
    const newClient = async (who, { isolated = false } = {}) => {
      const target = isolated ? await browser.createBrowserContext() : browser;
      if (isolated) {
        isolatedContexts.push(target);
      }
      const page = await target.newPage();
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

    // DIAG (ci-debug only): per-rid sender / receiver stats every 2 s.
    const diagStats = async (page, who) => {
      try {
        const out = await page.evaluate(async () => {
          const rows = [];
          (await window.__huddlePc.getStats()).forEach((r) => {
            if ((r.type === 'outbound-rtp' || r.type === 'inbound-rtp') && r.kind === 'video') {
              rows.push({
                t: r.type === 'outbound-rtp' ? 'out' : 'in', ssrc: r.ssrc, rid: r.rid, active: r.active,
                sent: r.framesSent, enc: r.framesEncoded, kfEnc: r.keyFramesEncoded, w: r.frameWidth, fps: r.framesPerSecond,
                q: r.qualityLimitationReason, qd: r.qualityLimitationDurations, tgt: r.targetBitrate,
                recv: r.framesReceived, dec: r.framesDecoded, kfDec: r.keyFramesDecoded, drop: r.framesDropped,
                pli: r.pliCount, fir: r.firCount, nack: r.nackCount, lost: r.packetsLost, freeze: r.freezeCount,
                bytes: r.bytesSent ?? r.bytesReceived, pkts: r.packetsSent ?? r.packetsReceived,
              });
            }
          });
          return rows;
        });
        console.log(`[diag ${who} ${new Date().toISOString()}] ${JSON.stringify(out)}`);
      } catch (e) {
        console.log(`[diag ${who}] failed: ${e}`);
      }
    };
    const diagTimer = setInterval(() => {
      diagStats(alice, 'alice');
      diagStats(bob, 'bob');
    }, 2000);
    diagTimer.unref();
    process.on('exit', () => clearInterval(diagTimer));
    console.log('alice turns blur on mid-call');
    const baselineSsrcs = await videoInboundSnapshot(bob);
    const baselineLocalTrack = await localVideoTrackId(alice);
    const baselineSdpM = await alice.evaluate(() =>
      window.__huddlePc.localDescription.sdp.split('\n').filter((l) => l.startsWith('m=')).length,
    );
    await alice.click('[data-testid="toggle-blur"]');
    let stats = null;
    await waitFor(
      'blur pipeline to activate',
      async () => {
        stats = await blurStats(alice);
        return stats?.active === true;
      },
      60_000,
    ).catch(async () => {
      await stateDump(alice, 'alice').catch(() => {});
      await stateDump(bob, 'bob').catch(() => {});
      fail(`alice: blur never activated (stats: ${JSON.stringify(stats)}, status: ${await statusText(alice)})`);
    });
    // The pipeline must produce frames: the shim's framesOut counter
    // must move, or the canvas track is a black hole.
    const framesAtOn = stats.framesOut;
    stats = await waitFor(
      'the blur pipeline to produce frames',
      async () => {
        const s = await blurStats(alice);
        return s?.framesOut > framesAtOn ? s : false;
      },
      30_000,
    );
    console.log(`alice: blur on (framesIn ${stats.framesIn}, framesOut ${stats.framesOut}, avgMs ${stats.avgMs})`);

    console.log('asserting the swap kept the same ssrcs decoding (no renegotiation)');
    // replaceTrack keeps the m-line: bob decodes alice's camera on the
    // SAME ssrc, and the frame count keeps growing — blur frames are
    // decodable media, and no SDP churn happened.
    await waitFor(
      'the same ssrcs to keep decoding after the swap',
      async () => {
        const snap = await videoInboundSnapshot(bob);
        const sameKeys = Object.keys(baselineSsrcs).every((s) => s in snap);
        const grew = Object.keys(baselineSsrcs).every((s) => (snap[s] ?? 0) > baselineSsrcs[s]);
        return sameKeys && grew ? snap : false;
      },
      60_000,
    ).then(async (snap) => {
      const nowKeys = Object.keys(snap);
      const baseKeys = Object.keys(baselineSsrcs);
      if (nowKeys.length !== baseKeys.length) {
        fail(
          `bob: the ssrc set changed with blur on ` +
            `(before: ${baseKeys}, after: ${nowKeys}) — blur must swap tracks, not renegotiate`,
        );
      }
      console.log(`bob: same ssrcs ${nowKeys.join(', ')} still decoding after the blur swap`);
    }).catch(async (e) => {
      await stateDump(bob, 'bob').catch(() => {});
      await diagStats(alice, 'alice-final');
      await diagStats(bob, 'bob-final');
      try {
        const m = await (await fetch(`${BASE}/metrics`)).text();
        console.log('[diag metrics]\n' + m.split('\n').filter((l) => /simulcast|keyframes|frames_total|bitrate/.test(l)).join('\n'));
      } catch (err) {
        console.log(`[diag metrics] failed: ${err}`);
      }
      throw e;
    });
    const afterSdpM = await alice.evaluate(() =>
      window.__huddlePc.localDescription.sdp.split('\n').filter((l) => l.startsWith('m=')).length,
    );
    if (afterSdpM !== baselineSdpM) {
      fail(`alice: m-line count changed with blur on (${baselineSdpM} -> ${afterSdpM}) — renegotiation happened`);
    }
    const blurredLocalTrack = await localVideoTrackId(alice);
    if (!blurredLocalTrack || blurredLocalTrack === baselineLocalTrack) {
      fail(
        `alice: the local tile's video track id did not change with blur on ` +
          `(${baselineLocalTrack} -> ${blurredLocalTrack})`,
      );
    }
    console.log(`alice: local tile now shows the processed track (${blurredLocalTrack})`);

    console.log('alice turns blur off mid-call');
    const beforeOffSsrcs = await videoInboundSnapshot(bob);
    await alice.click('[data-testid="toggle-blur"]');
    await waitFor(
      'blur pipeline to deactivate',
      async () => {
        stats = await blurStats(alice);
        return stats?.active === false;
      },
      60_000,
    ).catch(async () => {
      await stateDump(alice, 'alice').catch(() => {});
      fail(`alice: blur never deactivated (stats: ${JSON.stringify(stats)})`);
    });
    await waitFor(
      'the same ssrcs to keep decoding after turning blur off',
      async () => {
        const snap = await videoInboundSnapshot(bob);
        const sameKeys = Object.keys(beforeOffSsrcs).every((s) => s in snap);
        const grew = Object.keys(beforeOffSsrcs).every((s) => (snap[s] ?? 0) > beforeOffSsrcs[s]);
        return sameKeys && grew ? snap : false;
      },
      60_000,
    ).catch(async () => {
      await stateDump(bob, 'bob').catch(() => {});
      fail(`bob: decoding broke when alice turned blur off (before: ${JSON.stringify(beforeOffSsrcs)})`);
    });
    const rawLocalTrack = await localVideoTrackId(alice);
    if (!rawLocalTrack || rawLocalTrack === blurredLocalTrack) {
      fail(
        `alice: the local tile's video track id did not change back with blur off ` +
          `(${blurredLocalTrack} -> ${rawLocalTrack})`,
      );
    }
    console.log(`bob: still decoding on the same ssrcs after blur off; local tile back to the raw track`);

    console.log('erin enables blur in prejoin and joins');
    const erin = await newClient('erin');
    await erin.goto(`${BASE}/m/${code}?test=1`, { waitUntil: 'domcontentloaded' });
    await erin
      .waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
      .catch(async () => fail(`erin: media never became ready (status: ${await statusText(erin)})`));
    const prejoinTrackBefore = await erin.evaluate(
      () => document.querySelector('#preview')?.srcObject?.getVideoTracks()[0]?.id ?? null,
    );
    await erin.evaluate(() => document.querySelector('[data-testid="prejoin-toggle-blur"]')?.click());
    let erinStats = null;
    await waitFor(
      'erin: prejoin blur to activate',
      async () => {
        erinStats = await blurStats(erin);
        return erinStats?.active === true;
      },
      60_000,
    ).catch(async () =>
      fail(`erin: prejoin blur never activated (stats: ${JSON.stringify(erinStats)}, notice: ${erinStats?.notice})`),
    );
    const prejoinTrackAfter = await erin.evaluate(
      () => document.querySelector('#preview')?.srcObject?.getVideoTracks()[0]?.id ?? null,
    );
    if (!prejoinTrackAfter || prejoinTrackAfter === prejoinTrackBefore) {
      fail(
        `erin: the preview's video track id did not change with blur on ` +
          `(${prejoinTrackBefore} -> ${prejoinTrackAfter})`,
      );
    }
    console.log(`erin: prejoin preview shows the processed track (${prejoinTrackAfter})`);
    await erin.type('[data-testid="name-input"]', 'erin');
    await erin.evaluate(() => document.querySelector('[data-testid="join-btn"]')?.click());
    await erin
      .waitForSelector('[data-testid="name-input"]', { hidden: true, timeout: 30_000 })
      .catch(async () => fail(`erin never left the prejoin page (status: ${await statusText(erin)})`));
    // The prejoin choice must carry into the call: the in-call button
    // reads the persisted state, and the pipeline stays up (join
    // publishes the outgoing stream from the start).
    await erin
      .waitForFunction(
        () => document.querySelector('[data-testid="toggle-blur"]')?.textContent.includes('Blur on'),
        { timeout: 30_000, polling: 250 },
      )
      .catch(async () =>
        fail(`erin: in-call blur button lost the prejoin choice (status: ${await statusText(erin)})`),
      );
    await waitFor(
      "bob to receive erin's (blurred) media",
      async () => {
        const probe = await mediaProbe(bob, 'erin');
        return probe.hasStream && probe.audioBytes > 0 && probe.videoBytes > 0 ? probe : false;
      },
      60_000,
    ).catch(async () => {
      await stateDump(bob, 'bob').catch(() => {});
      await stateDump(erin, 'erin').catch(() => {});
      const probe = await mediaProbe(bob, 'erin').catch(() => null);
      fail(`bob: no media arriving from erin (blurred join): ${JSON.stringify(probe)}`);
    });
    console.log("bob receives erin's media — blur carried into the call without renegotiation");

    console.log('erin reloads the prejoin page: the choice persisted');
    // A fresh page load re-seeds the toggle from localStorage; erin is
    // still in the call on the old page, so use a brand-new page.
    const erin2 = await newClient('erin2');
    await erin2.goto(`${BASE}/m/${code}?test=1`, { waitUntil: 'domcontentloaded' });
    await erin2
      .waitForSelector('[data-testid="join-btn"]:not([disabled])', { timeout: 30_000 })
      .catch(async () => fail(`erin2: media never became ready`));
    const persistedLabel = await erin2.$eval('[data-testid="prejoin-toggle-blur"]', (el) => el.textContent);
    if (!persistedLabel.includes('Blur on')) {
      fail(`erin2: the blur preference did not survive a reload (button reads "${persistedLabel}")`);
    }
    // Auto-restore is async: the model loads only after getUserMedia
    // resolves, which can take a while under software WebGL.
    await waitFor(
      'erin2: the pipeline to auto-restore from the persisted preference',
      async () => (await blurStats(erin2))?.active === true,
      60_000,
    ).catch(async () => {
      fail(`erin2: the pipeline did not auto-restore from the persisted preference (stats: ${JSON.stringify(await blurStats(erin2))})`);
    });
    console.log(`erin2: prejoin reloaded with blur still on (label "${persistedLabel.trim()}", stats active)`);
    await erin2.close();

    console.log('frank hits the model-failure degrade path (blur-fail knob)');
    const frank = await newClient('frank', { isolated: true });
    await joinAs(frank, code, 'frank', '&blur-fail=1');
    await waitTile(bob, '[data-testid="tile-frank"]', 'bob');
    await frank.click('[data-testid="toggle-blur"]');
    let frankStats = null;
    await waitFor(
      "frank's failed toggle to surface",
      async () => {
        frankStats = await blurStats(frank);
        return frankStats?.active === false && !!frankStats?.error;
      },
      30_000,
    ).catch(async () => {
      await stateDump(frank, 'frank').catch(() => {});
      fail(`frank: model failure did not auto-disable with a notice (stats: ${JSON.stringify(frankStats)})`);
    });
    const frankStatus = await statusText(frank);
    if (!/could not be loaded/.test(frankStatus)) {
      fail(`frank: the model failure did not surface in the status line (status: "${frankStatus}")`);
    }
    // A failed toggle is a UI hiccup, never a call ender: the button
    // reads off and media keeps flowing.
    await frank
      .waitForFunction(
        () => document.querySelector('[data-testid="toggle-blur"]')?.textContent.includes('Blur off'),
        { timeout: 10_000, polling: 250 },
      )
      .catch(() => fail('frank: the blur button did not revert after the failed toggle'));
    const frankProbe = await waitFor(
      "bob to keep receiving frank's media after the failed toggle",
      async () => {
        const probe = await mediaProbe(bob, 'frank');
        return probe.videoBytes > 0 ? probe : false;
      },
      60_000,
    ).catch(async () => {
      await stateDump(bob, 'bob').catch(() => {});
      fail(`bob: frank's media broke after a failed blur toggle: ${JSON.stringify(await mediaProbe(bob, 'frank').catch(() => null))}`);
    });
    console.log(`frank: degrade path green — notice shown, call intact (${JSON.stringify(frankProbe)})`);

    console.log('gina hits the slow-frames degrade path (blur-slow knob)');
    const gina = await newClient('gina', { isolated: true });
    await joinAs(gina, code, 'gina', '&blur-slow=1');
    await waitTile(bob, '[data-testid="tile-gina"]', 'bob');
    await gina.click('[data-testid="toggle-blur"]');
    let ginaStats = null;
    await waitFor(
      "gina's pipeline to activate (slow)",
      async () => {
        ginaStats = await blurStats(gina);
        return ginaStats?.active === true;
      },
      60_000,
    ).catch(async () => {
      await stateDump(gina, 'gina').catch(() => {});
      fail(`gina: blur never activated with the slow knob (stats: ${JSON.stringify(ginaStats)})`);
    });
    // 30 consecutive slow frames at ~60ms each ≈ 2s until the shim
    // declares the pipeline too slow and auto-disables.
    await waitFor(
      'the slow pipeline to auto-disable',
      async () => {
        ginaStats = await blurStats(gina);
        return ginaStats?.active === false && ginaStats?.degraded === true;
      },
      60_000,
    ).catch(async () => {
      await stateDump(gina, 'gina').catch(() => {});
      fail(`gina: the slow pipeline never auto-disabled (stats: ${JSON.stringify(ginaStats)})`);
    });
    const ginaStatus = await statusText(gina);
    if (!/keep video smooth/.test(ginaStatus)) {
      fail(`gina: the auto-disable reason did not surface in the status line (status: "${ginaStatus}")`);
    }
    await gina
      .waitForFunction(
        () => document.querySelector('[data-testid="toggle-blur"]')?.textContent.includes('Blur off'),
        { timeout: 10_000, polling: 250 },
      )
      .catch(() => fail('gina: the blur button did not revert after the auto-disable'));
    const ginaProbe = await waitFor(
      "bob to keep receiving gina's media after the auto-disable",
      async () => {
        const probe = await mediaProbe(bob, 'gina');
        return probe.videoBytes > 0 ? probe : false;
      },
      60_000,
    ).catch(async () => {
      await stateDump(bob, 'bob').catch(() => {});
      fail(`bob: gina's media broke after the auto-disable: ${JSON.stringify(await mediaProbe(bob, 'gina').catch(() => null))}`);
    });
    console.log(`gina: slow-frame degrade green — notice shown, call intact (${JSON.stringify(ginaProbe)})`);

    console.log('PASS: background blur e2e green');
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
