// Huddle E2EE media transform worker (issue #28, ADR 011, D1-D5).
//
// One worker per call. Every RTCRtpScriptTransform (one per sender and
// receiver track) is bound to this worker; the `rtctransform` event
// fires here with { readable, writable, options }, where options is the
// { op: 'encrypt' | 'decrypt', kind: 'audio' | 'video' } dict the main
// thread passed at construction.
//
// SFrame framing (RFC 9605) with the fixed header form:
//   clear codec prefix | 0xBE | kid_be32 | ctr_be56 | AES-GCM ct+tag
// AAD = the 12-byte header; nonce = salt XOR ctr (zero-extended to 12
// bytes). The key/salt bytes arrive via postMessage (derived on the
// main thread in Rust, pinned against the RFC test vectors); this
// worker only imports them as CryptoKeys and encrypts/decrypts frames.
//
// Failure policy is drop-not-plaintext (ADR 011, D4): frames are
// dropped until keys exist or on any decrypt failure — nothing ever
// leaves this pipeline unencrypted in an E2EE meeting.
//
// Test knobs (ADR 011, D10), enabled via `?test=1` on the page:
//   tagOnly    — append/strip a 4-byte tag instead of real crypto, so
//                a plaintext meeting proves the whole transform + SFU
//                plumbing (`?e2eeTag=1`).
//   forceDegrade — pretend every transform is slow, tripping the EMA
//                degrade detector deterministically (`?e2eeDegrade=1`).
'use strict';

const SFRAME_CONFIG = 0xbe; // extended KID (4 bytes) + extended CTR (7 bytes)
const HEADER_LEN = 12;
const TAG = [0xde, 0xad, 0xb3, 0x3f]; // the tagOnly knob's marker

// kid -> { key: CryptoKey, salt: Uint8Array }
const keyTable = new Map();
let sendKid = null;
let sendCtr = 0;
let keysVersion = 0;
const mode = { tagOnly: false, forceDegrade: false };

// Stats surfaced to the e2e probe (`__huddleE2eeStats.transform`).
const stats = {
  attached: 0,
  encryptedFrames: 0,
  decryptedFrames: 0,
  decryptFail: 0,
  // Frames dropped because their KID had no key material YET — the
  // expected transient during epoch key distribution (a sender can switch
  // to a new epoch before every receiver has imported the new key). These
  // drops are by design; `decryptFail` counts only frames that failed
  // WITH known key material (genuine corruption/auth failures).
  pendingKeyMiss: 0,
  emaMs: 0,
  degraded: false,
};
let slowStreak = 0;
let framesSincePost = 0;

function post(message) {
  self.postMessage(message);
}

function postStats() {
  post({ t: 'stats', attached: stats.attached, encryptedFrames: stats.encryptedFrames,
    decryptedFrames: stats.decryptedFrames, decryptFail: stats.decryptFail,
    pendingKeyMiss: stats.pendingKeyMiss,
    emaMs: Math.round(stats.emaMs * 100) / 100, degraded: stats.degraded });
}

// EMA degrade detection (ADR 011, D5): >20ms average for 30
// consecutive frames posts a one-shot degrade notice.
function trackMs(ms) {
  if (mode.forceDegrade) ms = 50;
  stats.emaMs = stats.emaMs === 0 ? ms : stats.emaMs * 0.9 + ms * 0.1;
  if (stats.emaMs > 20) {
    slowStreak += 1;
    if (slowStreak >= 30 && !stats.degraded) {
      stats.degraded = true;
      post({ t: 'degrade' });
    }
  } else {
    slowStreak = 0;
  }
  framesSincePost += 1;
  if (framesSincePost >= 25) {
    framesSincePost = 0;
    postStats();
  }
}

async function importKey(raw) {
  return crypto.subtle.importKey('raw', raw, 'AES-GCM', false, ['encrypt', 'decrypt']);
}

// Full-table replace, ordered by version so a slow post never
// overwrites a newer one.
self.onmessage = async (event) => {
  const msg = event.data;
  if (msg.t === 'config') {
    mode.tagOnly = !!msg.tagOnly;
    mode.forceDegrade = !!msg.forceDegrade;
    return;
  }
  if (msg.t !== 'keys' || typeof msg.version !== 'number' || msg.version <= keysVersion) {
    return;
  }
  try {
    const entries = await Promise.all((msg.recv || []).map(async (entry) => [
      entry.kid, { key: await importKey(entry.key), salt: new Uint8Array(entry.salt) },
    ]));
    let send = null;
    if (msg.send) {
      send = [msg.send.kid, { key: await importKey(msg.send.key), salt: new Uint8Array(msg.send.salt) }];
    }
    // The importKey awaits above interleave with later onmessage calls;
    // re-check before committing so a slow, older completion cannot
    // overwrite a newer table.
    if (msg.version <= keysVersion) {
      return;
    }
    const next = new Map(entries);
    if (send) next.set(send[0], send[1]);
    keyTable.clear();
    for (const [kid, material] of next) keyTable.set(kid, material);
    keysVersion = msg.version;
    if (send) {
      if (sendKid !== send[0]) { sendKid = send[0]; sendCtr = 0; }
    } else {
      sendKid = null;
      sendCtr = 0;
    }
  } catch (e) {
    // A malformed table leaves the previous one in place.
  }
};

// The header-clear convention (ADR 011, D3): the codec's own framing
// bytes stay in the clear so the SFU's VP8 keyframe classification
// keeps reading the true first payload byte. VP8: 10 bytes for a
// keyframe, 3 for a delta (P bit = 0 marks the keyframe form); Opus:
// the 1-byte TOC.
function clearPrefixLen(data, kind) {
  if (kind === 'video') return (data[0] & 0x01) === 0 ? 10 : 3;
  return 1;
}

function buildHeader(kid, ctr) {
  const header = new Uint8Array(HEADER_LEN);
  header[0] = SFRAME_CONFIG;
  header[1] = (kid >>> 24) & 0xff;
  header[2] = (kid >>> 16) & 0xff;
  header[3] = (kid >>> 8) & 0xff;
  header[4] = kid & 0xff;
  // 7-byte big-endian CTR (a JS number is exact far beyond this).
  let v = ctr;
  for (let i = HEADER_LEN - 1; i >= 5; i -= 1) {
    header[i] = v & 0xff;
    v = Math.floor(v / 256);
  }
  return header;
}

// nonce = salt XOR BE(ctr, 12) — the CTR's bytes sit at header[5..12].
function nonceFor(salt, header) {
  const nonce = new Uint8Array(12);
  for (let i = 0; i < 12; i += 1) nonce[i] = salt[i] ^ (i < 5 ? 0 : header[i]);
  return nonce;
}

async function encryptFrame(frame, kind) {
  const t0 = performance.now();
  const data = new Uint8Array(frame.data);
  if (!mode.tagOnly && sendKid === null) {
    // Drop until the send key arrives — never forward in the clear.
    return null;
  }
  if (mode.tagOnly) {
    const out = new Uint8Array(data.length + TAG.length);
    out.set(data);
    out.set(TAG, data.length);
    frame.data = out.buffer;
    stats.encryptedFrames += 1;
    trackMs(performance.now() - t0);
    return frame;
  }
  const material = keyTable.get(sendKid);
  if (!material) return null;
  const clear = clearPrefixLen(data, kind);
  const header = buildHeader(sendKid, sendCtr);
  sendCtr += 1;
  const nonce = nonceFor(material.salt, header);
  const gcm = { name: 'AES-GCM', iv: nonce, additionalData: header, tagLength: 128 };
  const ct = new Uint8Array(await crypto.subtle.encrypt(gcm, material.key, data.subarray(clear)));
  const out = new Uint8Array(clear + HEADER_LEN + ct.length);
  out.set(data.subarray(0, clear), 0);
  out.set(header, clear);
  out.set(ct, clear + HEADER_LEN);
  frame.data = out.buffer;
  stats.encryptedFrames += 1;
  trackMs(performance.now() - t0);
  return frame;
}

async function decryptFrame(frame, kind) {
  const t0 = performance.now();
  const data = new Uint8Array(frame.data);
  if (mode.tagOnly) {
    if (data.length >= TAG.length + 1) {
      const tail = data.subarray(data.length - TAG.length);
      if (TAG.every((byte, i) => tail[i] === byte)) {
        // slice() copies to an exact-size buffer (a subarray's .buffer
        // would still span the full original frame).
        frame.data = data.slice(0, data.length - TAG.length).buffer;
        stats.decryptedFrames += 1;
        trackMs(performance.now() - t0);
        return frame;
      }
    }
    stats.decryptFail += 1;
    return null;
  }
  const clear = clearPrefixLen(data, kind);
  if (data.length < clear + HEADER_LEN + 16 || data[clear] !== SFRAME_CONFIG) {
    stats.decryptFail += 1;
    trackMs(performance.now() - t0);
    return null;
  }
  const header = data.subarray(clear, clear + HEADER_LEN);
  const kid = ((header[1] << 24) | (header[2] << 16) | (header[3] << 8) | header[4]) >>> 0;
  const material = keyTable.get(kid);
  if (!material) {
    // No key material for this KID yet — expected while an epoch's sender
    // key is still in flight to us (sender switched before our import).
    // Drop (never plaintext) and count separately from genuine failures.
    stats.pendingKeyMiss += 1;
    trackMs(performance.now() - t0);
    return null;
  }
  const nonce = nonceFor(material.salt, header);
  const gcm = { name: 'AES-GCM', iv: nonce, additionalData: header, tagLength: 128 };
  try {
    const pt = await crypto.subtle.decrypt(gcm, material.key, data.subarray(clear + HEADER_LEN));
    const out = new Uint8Array(clear + pt.byteLength);
    out.set(data.subarray(0, clear), 0);
    out.set(new Uint8Array(pt), clear);
    frame.data = out.buffer;
    stats.decryptedFrames += 1;
    trackMs(performance.now() - t0);
    return frame;
  } catch (e) {
    stats.decryptFail += 1;
    trackMs(performance.now() - t0);
    return null;
  }
}

// One pipeline per attached transform (RTCRtpScriptTransform instances
// are not reusable): readable -> transform -> writable.
self.onrtctransform = (event) => {
  const { readable, writable, options } = event.transformer;
  stats.attached += 1;
  framesSincePost += 1; // surface the attach promptly in the stats post cadence
  const op = options && options.op;
  const kind = (options && options.kind) || 'video';
  const pipeline = new TransformStream({
    transform: async (frame, controller) => {
      try {
        const out = op === 'encrypt'
          ? await encryptFrame(frame, kind)
          : await decryptFrame(frame, kind);
        if (out) controller.enqueue(out);
      } catch (e) {
        // Drop on any unexpected failure — never forward in the clear.
      }
    },
  });
  readable.pipeThrough(pipeline).pipeTo(writable);
};
