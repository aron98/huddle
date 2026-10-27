#!/usr/bin/env node
// Magic-link auth + scheduling dashboard e2e (phase 5).
//
// Boots real huddle servers and asserts, against a real browser:
//   - anonymous meeting creation via the API still works,
//   - the dashboard asks anonymous visitors to sign in,
//   - a magic link is issued (logged when SMTP is unconfigured), lands a
//     session cookie, and the browser renders the organizer dashboard,
//   - meetings can be created from the dashboard UI, exported as ICS,
//     and cancelled,
//   - ownership is enforced (anonymous and non-owner PATCH → 403),
//   - OIDC SSO (issue #11): with a second server configured against a
//     fake issuer, the SSO routes 404 on the magic-link server, magic
//     links 409 on the SSO server, and a browser login lands on the
//     same dashboard with the same session cookie. Opening /login on
//     the SSO server goes straight to the IdP (issue #99).

import { spawn } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createHash, createSign, generateKeyPairSync, randomBytes } from 'node:crypto';
import http from 'node:http';
import puppeteer from 'puppeteer-core';

const PORT = 3132;
const BASE = `http://127.0.0.1:${PORT}`;
// The OIDC-configured instance: 3131-3133 may be in use by sibling suites.
const OIDC_PORT = 3134;
const OIDC_BASE = `http://127.0.0.1:${OIDC_PORT}`;
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

async function waitFor(what, fn, timeoutMs = 20_000, everyMs = 500) {
  const deadline = Date.now() + timeoutMs;
  let last;
  for (;;) {
    last = await fn();
    if (last) return last;
    if (Date.now() > deadline) fail(`timed out waiting for ${what}: ${JSON.stringify(last)}`);
    await delay(everyMs);
  }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

async function bootServer(port, base, extraEnv = {}) {
  const dataDir = mkdtempSync(join(tmpdir(), 'huddle-auth-e2e-'));
  const server = spawn(SERVER, [], {
    cwd: ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      RUST_LOG: process.env.RUST_LOG ?? 'huddle=info',
      HUDDLE_BIND: `127.0.0.1:${port}`,
      // Magic links and OIDC redirects are built from the configured
      // base URL — point it at this instance.
      HUDDLE_BASE_URL: base,
      HUDDLE_PUBLIC_IP: '127.0.0.1',
      HUDDLE_MEDIA_PORT: '0',
      HUDDLE_DATA_DIR: dataDir,
      HUDDLE_WEB_DIR: join(ROOT, 'crates/web/dist'),
      ...extraEnv,
    },
  });
  let logs = '';
  server.stdout.on('data', (d) => (logs += d));
  server.stderr.on('data', (d) => (logs += d));

  await waitFor('server health', async () => {
    try {
      const res = await fetch(`${base}/api/health`);
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

const rfc3339 = (date) => date.toISOString().replace(/\.\d+Z$/, 'Z');

// ---------------------------------------------------------------------------
// Fake OIDC issuer
// ---------------------------------------------------------------------------

// A minimal OIDC issuer: discovery, JWKS, an /authorize that immediately
// codes back, and a /token that enforces PKCE (S256), client
// credentials, and single-use codes. RS256 because node's crypto signs
// that shape natively.
async function startFakeIdp(clientId, clientSecret, email) {
  const { publicKey, privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
  const jwk = publicKey.export({ format: 'jwk' });
  const codes = new Map(); // code -> { challenge, nonce, used }
  const b64u = (buf) => Buffer.from(buf).toString('base64url');

  const idp = http.createServer((req, res) => {
    const base = `http://${req.headers.host}`;
    const url = new URL(req.url, base);
    const sendJson = (status, body) => {
      res.writeHead(status, { 'content-type': 'application/json' });
      res.end(JSON.stringify(body));
    };

    if (req.method === 'GET' && url.pathname === '/.well-known/openid-configuration') {
      return sendJson(200, {
        issuer: base,
        authorization_endpoint: `${base}/authorize`,
        token_endpoint: `${base}/token`,
        jwks_uri: `${base}/jwks`,
      });
    }
    if (req.method === 'GET' && url.pathname === '/jwks') {
      return sendJson(200, {
        keys: [{ kty: 'RSA', n: jwk.n, e: jwk.e, alg: 'RS256', use: 'sig' }],
      });
    }
    if (req.method === 'GET' && url.pathname === '/authorize') {
      const code = randomBytes(16).toString('hex');
      codes.set(code, {
        challenge: url.searchParams.get('code_challenge'),
        nonce: url.searchParams.get('nonce'),
        used: false,
      });
      const redirect = `${url.searchParams.get('redirect_uri')}?code=${code}` +
        `&state=${encodeURIComponent(url.searchParams.get('state'))}`;
      res.writeHead(302, { location: redirect });
      return res.end();
    }
    if (req.method === 'POST' && url.pathname === '/token') {
      let raw = '';
      req.on('data', (chunk) => (raw += chunk));
      req.on('end', () => {
        const form = new URLSearchParams(raw);
        const record = codes.get(form.get('code'));
        if (!record || record.used) return sendJson(400, { error: 'invalid_grant' });
        if (form.get('client_id') !== clientId || form.get('client_secret') !== clientSecret) {
          return sendJson(401, { error: 'invalid_client' });
        }
        const challenge = createHash('sha256')
          .update(form.get('code_verifier') ?? '')
          .digest('base64url');
        if (challenge !== record.challenge) return sendJson(400, { error: 'invalid_grant' });
        record.used = true;

        const now = Math.floor(Date.now() / 1000);
        const header = b64u(JSON.stringify({ alg: 'RS256', typ: 'JWT' }));
        const payload = b64u(JSON.stringify({
          iss: base,
          sub: `user-${email}`,
          aud: clientId,
          exp: now + 3600,
          iat: now,
          email,
          email_verified: true,
          nonce: record.nonce,
        }));
        const signer = createSign('RSA-SHA256');
        signer.update(`${header}.${payload}`);
        const idToken = `${header}.${payload}.${signer.sign(privateKey, 'base64url')}`;
        sendJson(200, { access_token: 'fake-access', token_type: 'Bearer', id_token: idToken });
      });
      return undefined;
    }
    return sendJson(404, { error: 'not found' });
  });

  await new Promise((resolveListen) => idp.listen(0, '127.0.0.1', resolveListen));
  return {
    base: `http://127.0.0.1:${idp.address().port}`,
    close: () => new Promise((r) => idp.close(r)),
  };
}

// ---------------------------------------------------------------------------
// Magic-link helpers
// ---------------------------------------------------------------------------

async function requestMagicLink(email) {
  const res = await fetch(`${BASE}/api/auth/request-link`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ email }),
  });
  if (!res.ok) fail(`request-link for ${email} returned ${res.status}`);
}

async function magicLinkFrom(logs) {
  // Take the LAST logged link: earlier magic links stay in the log, so a
  // fresh match on the first occurrence would replay a consumed token.
  return waitFor('magic link in server logs', async () => {
    const matches = logs().match(/https?:\/\/\S+\/api\/auth\/verify\?token=\S+/g);
    return matches ? matches.at(-1) : null;
  });
}

async function followLink(link) {
  const res = await fetch(link, { redirect: 'manual' });
  const cookie = res.headers.get('set-cookie');
  if (res.status !== 303 || !cookie || !cookie.startsWith('huddle_session=')) {
    fail(`verify link did not issue a session: status=${res.status} cookie=${cookie}`);
  }
  return cookie;
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

const browser = await puppeteer.launch({
  executablePath: CHROME,
  headless: true,
  args: ['--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage'],
});
const server = await bootServer(PORT, BASE);
let oidcServer = null;
let fakeIdp = null;

try {
  // Anonymous creation still works.
  const anonymous = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      title: 'Anonymous standup',
      starts_at: rfc3339(new Date(Date.now() + 10 * 60_000)),
      ends_at: rfc3339(new Date(Date.now() + 70 * 60_000)),
    }),
  });
  if (anonymous.status !== 201) fail(`anonymous create returned ${anonymous.status}`);
  console.log('anonymous API create still works');

  // The dashboard asks anonymous visitors to sign in.
  const page = await browser.newPage();
  page.on('pageerror', (error) => console.log(`[pageerror] ${error.message}`));
  await page.goto(`${BASE}/meetings`);
  await page.waitForSelector('[data-testid="signed-out"]', { timeout: 10_000 });
  console.log('anonymous dashboard shows signed-out');

  // Magic link for the test organizer (SMTP unconfigured → link is logged).
  await requestMagicLink('test@example.com');
  const link = await magicLinkFrom(server.logs);
  console.log(`magic link logged: ${link}`);

  // The browser follows the link: verify consumes the single-use token,
  // sets the session cookie, and redirects to /meetings where the
  // dashboard renders. (The link cannot be fetched twice — the second
  // use would be rejected — so the browser is the one to consume it.)
  await page.goto(link, { waitUntil: 'networkidle0' });
  await page.waitForSelector('[data-testid="create-btn"]', { timeout: 15_000 });
  console.log('magic link signed the browser in; dashboard renders');

  // Create a meeting from the UI. datetime-local inputs do not take typed
  // text reliably — set the value programmatically and dispatch input.
  await page.type('[data-testid="create-title"]', 'Quarterly review');
  await page.$eval('[data-testid="create-starts"]', (el, v) => {
    el.value = v;
    el.dispatchEvent(new Event('input', { bubbles: true }));
  }, '2026-10-15T10:00');
  await page.$eval('[data-testid="create-ends"]', (el, v) => {
    el.value = v;
    el.dispatchEvent(new Event('input', { bubbles: true }));
  }, '2026-10-15T11:00');
  await page.click('[data-testid="create-btn"]');

  const row = await page.waitForSelector('[data-testid^="meeting-row-"]', { timeout: 15_000 });
  const rowTestid = await row.evaluate((el) => el.getAttribute('data-testid'));
  const code = rowTestid.replace('meeting-row-', '');
  console.log(`created meeting ${code} from the dashboard`);

  // ICS export: correct type and content.
  const ics = await fetch(`${BASE}/api/meetings/${code}/ics`);
  const icsBody = await ics.text();
  if (ics.status !== 200) fail(`ICS download returned ${ics.status}`);
  const icsType = ics.headers.get('content-type') ?? '';
  if (!icsType.includes('text/calendar')) fail(`ICS content-type was ${icsType}`);
  if (!icsBody.includes('BEGIN:VCALENDAR')) fail('ICS body is missing BEGIN:VCALENDAR');
  if (!icsBody.includes('SUMMARY:Quarterly review')) fail('ICS body is missing the title');
  console.log('ICS export works');

  // Ownership: anonymous PATCH forbidden, owner PATCH works (via the
  // browser's session cookie, using same-origin fetch), non-owner forbidden.
  const patch = (title) => JSON.stringify({ title });
  const anonymousPatch = await fetch(`${BASE}/api/meetings/${code}`, {
    method: 'PATCH',
    headers: { 'content-type': 'application/json' },
    body: patch('hijack'),
  });
  if (anonymousPatch.status !== 403) fail(`anonymous PATCH returned ${anonymousPatch.status}`);

  const ownerPatchStatus = await page.evaluate(async (c) => {
    const res = await fetch(`/api/meetings/${c}`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ title: 'Quarterly review v2' }),
    });
    return res.status;
  }, code);
  if (ownerPatchStatus !== 200) fail(`owner PATCH returned ${ownerPatchStatus}`);

  await requestMagicLink('eve@example.com');
  const eveLink = await magicLinkFrom(server.logs);
  const eveCookie = await followLink(eveLink);
  const evePatch = await fetch(`${BASE}/api/meetings/${code}`, {
    method: 'PATCH',
    headers: { 'content-type': 'application/json', cookie: eveCookie },
    body: patch('eve hijack'),
  });
  if (evePatch.status !== 403) fail(`non-owner PATCH returned ${evePatch.status}`);
  console.log('ownership enforced');

  // Cancel from the dashboard; the row flips to the cancelled chip.
  await page.click(`[data-testid="cancel-${code}"]`);
  await page.waitForFunction(
    (c) => document.querySelector(`[data-testid="meeting-row-${c}"]`)?.textContent?.includes('cancelled'),
    { timeout: 15_000 },
    code,
  );
  console.log('cancel from the dashboard works');

  // -----------------------------------------------------------------
  // OIDC SSO (issue #11): provider selection at config level, then a
  // full browser login against the fake issuer.
  // -----------------------------------------------------------------

  // Without [oidc] configured, the SSO routes do not exist.
  const unconfigured = await fetch(`${BASE}/api/auth/oidc/login`, { redirect: 'manual' });
  if (unconfigured.status !== 404) {
    fail(`oidc login on the magic-link server returned ${unconfigured.status}, want 404`);
  }
  console.log('SSO routes are 404 when OIDC is unconfigured');

  // A second server, configured against the fake issuer.
  fakeIdp = await startFakeIdp('e2e-client', 'e2e-secret', 'sso@example.com');
  oidcServer = await bootServer(OIDC_PORT, OIDC_BASE, {
    HUDDLE_OIDC_ISSUER_URL: fakeIdp.base,
    HUDDLE_OIDC_CLIENT_ID: 'e2e-client',
    HUDDLE_OIDC_CLIENT_SECRET: 'e2e-secret',
  });

  // Magic links are replaced while SSO is configured: 409 + a pointer.
  const magic = await fetch(`${OIDC_BASE}/api/auth/request-link`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ email: 'someone@example.com' }),
  });
  const magicBody = await magic.json();
  if (magic.status !== 409 || magicBody.status !== 'oidc_configured') {
    fail(`request-link under OIDC returned ${magic.status} ${JSON.stringify(magicBody)}, want 409 oidc_configured`);
  }
  console.log('magic links answer 409 while SSO is configured');

  // The login route redirects to the issuer with PKCE, state, and nonce,
  // and binds the state to this browser with a cookie.
  const login = await fetch(`${OIDC_BASE}/api/auth/oidc/login`, { redirect: 'manual' });
  const loginCookie = login.headers.get('set-cookie') ?? '';
  if (login.status !== 303 || !loginCookie.startsWith('huddle_oidc_login=')) {
    fail(`oidc login returned ${login.status} cookie=${loginCookie}`);
  }
  const location = login.headers.get('location') ?? '';
  if (!location.startsWith(fakeIdp.base) || !location.includes('/authorize?')) {
    fail(`oidc login redirected to ${location}, want the fake issuer`);
  }
  const loginParams = new URL(location).searchParams;
  if (loginParams.get('code_challenge_method') !== 'S256' || !loginParams.get('code_challenge')) {
    fail(`login redirect lacks PKCE: ${location}`);
  }
  if (!loginParams.get('state') || !loginParams.get('nonce')) {
    fail(`login redirect lacks state/nonce: ${location}`);
  }
  console.log('oidc login redirects to the issuer with PKCE state and nonce');

  // The callback refuses a state this browser was not issued.
  const unbound = await fetch(
    `${OIDC_BASE}/api/auth/oidc/callback?code=x&state=${encodeURIComponent(loginParams.get('state'))}`,
    { redirect: 'manual' },
  );
  if (unbound.status !== 401) {
    fail(`callback without the binding cookie returned ${unbound.status}, want 401`);
  }
  console.log('callback requires the browser-bound state');

  // A real browser login: opening /login probes the sign-in method,
  // redirects straight to the issuer (issue #99), and the callback
  // lands on /meetings, with the same dashboard the magic-link flow
  // lands on.
  const ssoPage = await browser.newPage();
  ssoPage.on('pageerror', (error) => console.log(`[pageerror] ${error.message}`));
  await ssoPage.goto(`${OIDC_BASE}/login`, { waitUntil: 'networkidle0' });
  await ssoPage.waitForSelector('[data-testid="create-btn"]', { timeout: 15_000 });
  const who = await ssoPage.evaluate(async () => (await fetch('/api/auth/me')).json());
  if (who.email !== 'sso@example.com') {
    fail(`SSO session is ${JSON.stringify(who)}, want sso@example.com`);
  }
  await ssoPage.close();
  console.log('browser SSO login lands on the same dashboard with a session');

  console.log('PASS: magic link and dashboard e2e green');
} catch (error) {
  if (!(error instanceof FailSignal)) {
    console.error('FAIL: unexpected error');
    console.error(error);
    process.exitCode = 1;
  }
} finally {
  await browser.close().catch(() => {});
  oidcServer?.cleanup();
  await fakeIdp?.close().catch(() => {});
  server.cleanup();
  if (process.env.DUMP_SERVER_LOGS !== '0') {
    console.log('--- server logs (tail) ---');
    console.log(server.logs().slice(-4000));
  }
}

process.exit(process.exitCode ?? 0);
