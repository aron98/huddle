#!/usr/bin/env node
// Huddle recording muxer — the ffmpeg sidecar of issue #15.
//
// The server records meetings as length-framed raw RTP, one `.rtp` file
// per participant per source (audio / camera / screen) plus a `session.sdp`
// and a `manifest.json` track table, under
//   {data_dir}/recordings/{meeting_id}/{recording_id}/
// ffmpeg cannot read raw RTP from files directly, so this script feeds it
// the way it expects: every packet is replayed as a single UDP datagram to
// the loopback port its SDP m-line declares, while ffmpeg demuxes the SDP.
//
// Two passes:
//   1. demux:   ffmpeg -protocol_whitelist file,udp,rtp -f sdp -i session.sdp
//               with one `-c copy` .mkv output per track, fed by the replay;
//   2. render:  ffmpeg with filter_complex — video tiles (scale + xstack
//               grid) and mixed audio (amix) — into a single .mp4.
//
// Usage:
//   node mux.mjs <capture_dir> [output.mp4]
// or, as the compose sidecar runs it (env-driven):
//   MEETING_ID=<uuid> RECORDING_ID=<uuid> DATA_DIR=/workspace/data node mux.mjs
//
// HUDDLE_MUX_SPEED (default 8) replays the capture faster than real time;
// lower it if the demux drops datagrams on a very long recording.

import { spawn } from 'node:child_process';
import dgram from 'node:dgram';
import fs from 'node:fs';
import path from 'node:path';
import process from 'node:process';

const TILE_W = 640;
const TILE_H = 360;
const REPLAY_SPEED = Number(process.env.HUDDLE_MUX_SPEED || 8);

function usageAndExit() {
  console.error('usage: mux.mjs <capture_dir> [output.mp4]');
  console.error('   or: MEETING_ID=<uuid> RECORDING_ID=<uuid> [DATA_DIR=dir] mux.mjs [output.mp4]');
  process.exit(2);
}

function resolvePaths(args) {
  if (args.length >= 1 && !process.env.MEETING_ID) {
    return { capture: path.resolve(args[0]), output: path.resolve(args[1] || 'meeting.mp4') };
  }
  const meeting = process.env.MEETING_ID;
  const recording = process.env.RECORDING_ID;
  const dataDir = process.env.DATA_DIR || '/workspace/data';
  if (!meeting || !recording) usageAndExit();
  const capture = path.join(dataDir, 'recordings', meeting, recording);
  return { capture, output: path.resolve(args[0] || path.join(capture, 'meeting.mp4')) };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function fileSize(file) {
  try {
    return fs.statSync(file).size;
  } catch {
    return 0;
  }
}

/// Split a length-framed .rtp capture into packet buffers. Each record is
/// a u16 big-endian length followed by that many bytes of one RTP packet.
function readPackets(file) {
  const buf = fs.readFileSync(file);
  const packets = [];
  let off = 0;
  while (off + 2 <= buf.length) {
    const len = buf.readUInt16BE(off);
    off += 2;
    if (off + len > buf.length) break; // truncated tail: ignore
    packets.push(buf.subarray(off, off + len));
    off += len;
  }
  return packets;
}

/// Replay one track's packets as UDP datagrams, paced by the RTP
/// timestamps (scaled by REPLAY_SPEED) so the demux can keep up.
async function replayTrack(track, packets) {
  const socket = dgram.createSocket('udp4');
  const rate = track.kind === 'audio' ? 48000 : 90000;
  try {
    let prev = null;
    for (const pkt of packets) {
      if (pkt.length >= 8) {
        const ts = pkt.readUInt32BE(4);
        if (prev !== null) {
          const delta = (ts - prev) >>> 0; // u32 wrap-safe
          const ms = delta < 0x80000000 ? (delta / rate) * 1000 / REPLAY_SPEED : 0;
          if (ms > 0 && ms < 1000) await sleep(ms); // long gaps capped at 1s
        }
        prev = ts;
      }
      await new Promise((resolve, reject) =>
        socket.send(pkt, track.port, '127.0.0.1', (err) => (err ? reject(err) : resolve())));
    }
  } finally {
    await new Promise((resolve) => socket.close(resolve));
  }
}

function runFfmpeg(args, label) {
  return new Promise((resolve, reject) => {
    const child = spawn('ffmpeg', ['-hide_banner', '-loglevel', 'warning', ...args], {
      stdio: ['ignore', 'inherit', 'pipe'],
    });
    let stderr = '';
    child.stderr.on('data', (d) => { stderr += d.toString(); });
    child.on('error', reject);
    child.on('close', (code) =>
      code === 0 ? resolve() : reject(new Error(`${label} failed (exit ${code}):\n${stderr}`)));
  });
}

/// Pass 1: demux while the replay feeds ffmpeg's SDP sockets. ffmpeg's SDP
/// input never reaches EOF, so the replay signals completion by sending it
/// SIGINT once every packet has been sent and drained.
async function demuxTracks(capture, tracks, workDir) {
  const outputs = tracks.map((_, i) => path.join(workDir, `track-${i}.mkv`));
  const args = [
    '-nostdin',
    '-protocol_whitelist', 'file,udp,rtp',
    '-f', 'sdp', '-i', path.join(capture, 'session.sdp'),
  ];
  tracks.forEach((_, i) => {
    args.push('-map', `0:${i}`, '-c', 'copy', outputs[i]);
  });

  const child = spawn('ffmpeg', ['-hide_banner', '-loglevel', 'warning', ...args], {
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  const ffmpegDone = new Promise((resolve) => child.on('close', resolve));

  // Give the demuxer a moment to bind its RTP sockets before sending.
  await sleep(500);
  const packets = tracks.map((t) => readPackets(path.join(capture, t.file)));
  await Promise.all(tracks.map((t, i) => replayTrack(t, packets[i])));
  await sleep(1000); // drain what the demuxer has buffered
  child.kill('SIGINT');
  const code = await ffmpegDone;
  if (code !== 0 && code !== 255) {
    throw new Error(`demux pass failed (exit ${code})`);
  }
  return outputs;
}

/// Pass 2: tiles + amix. The grid is square-ish; xstack positions every
/// tile at (col * TILE_W, row * TILE_H).
async function renderFinal(capture, tracks, outputs, output) {
  // `index` is the track's demux stream number — pass-2 inputs are listed
  // in the same m-line order, so it is also the pass-2 `-i` index.
  const usable = tracks
    .map((track, i) => ({ track, file: outputs[i], index: i }))
    .filter((x) => fileSize(x.file) > 0);
  if (usable.length === 0) {
    throw new Error(`the demuxer produced no playable track for ${capture}`);
  }

  const videos = usable.filter((x) => x.track.kind === 'video');
  const audios = usable.filter((x) => x.track.kind === 'audio');
  const filters = [];
  let vout = null;
  let aout = null;

  if (videos.length > 0) {
    videos.forEach((v, k) => {
      filters.push(`[${v.index}:v]scale=${TILE_W}:${TILE_H},setsar=1[vt${k}]`);
    });
    if (videos.length === 1) {
      vout = '[vt0]';
    } else {
      const cols = Math.ceil(Math.sqrt(videos.length));
      const layout = videos
        .map((_, k) => `${(k % cols) * TILE_W}_${Math.floor(k / cols) * TILE_H}`)
        .join('|');
      const inputs = videos.map((_, k) => `[vt${k}]`).join('');
      filters.push(`${inputs}xstack=inputs=${videos.length}:layout=${layout}[vout]`);
      vout = '[vout]';
    }
  }
  if (audios.length > 0) {
    if (audios.length === 1) {
      aout = `[${audios[0].index}:a]`;
    } else {
      const labels = audios.map((a) => `[${a.index}:a]`).join('');
      filters.push(`${labels}amix=inputs=${audios.length}:duration=longest[aout]`);
      aout = '[aout]';
    }
  }

  const args = [];
  usable.forEach((x) => args.push('-i', x.file));
  if (filters.length > 0) args.push('-filter_complex', filters.join(';'));
  if (vout) args.push('-map', vout);
  if (aout) args.push('-map', aout);
  if (vout) args.push('-c:v', 'libx264', '-preset', 'veryfast', '-pix_fmt', 'yuv420p');
  if (aout) args.push('-c:a', 'aac');
  args.push(path.resolve(output));
  await runFfmpeg(args, 'render pass');
  console.log(`wrote ${path.resolve(output)}`);
}

async function main() {
  const { capture, output } = resolvePaths(process.argv.slice(2));
  const manifest = JSON.parse(fs.readFileSync(path.join(capture, 'manifest.json'), 'utf8'));
  const all = manifest.tracks;
  if (!Array.isArray(all) || all.length === 0) {
    throw new Error(`manifest.json in ${capture} lists no tracks`);
  }
  // The SDP lists every track (empty ones included) and the demuxer
  // assigns stream indices in m-line order, so replay *all* of them —
  // empty files simply have no packets to send.
  const empty = all.filter((t) => fileSize(path.join(capture, t.file)) === 0);
  if (empty.length === all.length) {
    throw new Error(`capture ${capture} has no media`);
  }
  empty.forEach((t) => console.warn(`skipping empty track ${t.file}`));

  const workDir = path.join(capture, 'mux-work');
  fs.mkdirSync(workDir, { recursive: true });
  console.log(`muxing ${all.length} track(s) from ${capture}`);
  const outputs = await demuxTracks(capture, all, workDir);
  await renderFinal(capture, all, outputs, output);
}

main().catch((err) => {
  console.error(`mux failed: ${err && err.message ? err.message : err}`);
  process.exit(1);
});
