#!/usr/bin/env node
// Agent access e2e (issue #9): OAuth device flow, CLI, and MCP server.
//
// Boots a real huddle server and asserts, against a real browser and the
// real huddle-cli binary:
//   - `huddle-cli login` runs the RFC 8628 device flow end to end: the
//     CLI prints its user code, the browser (already signed in via a
//     magic link) approves it on the server-rendered /oauth/device page,
//     and the CLI stores a refresh token,
//   - the CLI can create, list, get, export as ICS, and cancel meetings
//     through its bearer token (all with --json),
//   - `huddle-cli mcp` speaks stdio JSON-RPC MCP: initialize,
//     tools/list, and tools/call for create/list/cancel,
//   - a read-only approval (scope meetings:read) is granted through the
//     same page but cannot create meetings (403),
//   - the dashboard lists authorizations and revoking one there kills
//     the CLI's refresh token immediately.

import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import puppeteer from 'puppeteer-core';

const PORT = 3140; // 3131-3139 are taken by sibling suites.
const BASE = `http://127.0.0.1:${PORT}`;
const CHROME = process.env.CHROME_BIN ?? 'google-chrome';
const ROOT = resolve(import.meta.dirname, '../..');
const SERVER = join(ROOT, 'target/debug/huddle');
const CLI = join(ROOT, 'target/debug/huddle-cli');
const EMAIL = 'agent@example.com';
const FORM = 'application/x-www-form-urlencoded';

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
// Server + CLI helpers
// ---------------------------------------------------------------------------

async function bootServer() {
  const dataDir = mkdtempSync(join(tmpdir(), 'huddle-agent-e2e-'));
  const server = spawn(SERVER, [], {
    cwd: ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: {
      ...process.env,
      RUST_LOG: process.env.RUST_LOG ?? 'huddle=info',
      HUDDLE_BIND: `127.0.0.1:${PORT}`,
      HUDDLE_BASE_URL: BASE,
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

// Run huddle-cli to completion and capture stdout/stderr/exit code.
function runCli(args, tokenFile) {
  return new Promise((resolveRun) => {
    const proc = spawn(CLI, args, {
      cwd: ROOT,
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, HUDDLE_CLI_TOKEN_FILE: tokenFile },
    });
    let stdout = '';
    let stderr = '';
    proc.stdout.on('data', (d) => (stdout += d));
    proc.stderr.on('data', (d) => (stderr += d));
    proc.on('close', (code) => resolveRun({ code, stdout, stderr, proc }));
  });
}

// Start `huddle-cli login --json` and resolve the FIRST stdout JSON line
// (user code + verification URL) while the process is still polling.
async function startCliLogin(tokenFile) {
  const proc = spawn(CLI, ['login', '--server', BASE, '--json'], {
    cwd: ROOT,
    stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, HUDDLE_CLI_TOKEN_FILE: tokenFile },
  });
  let stdout = '';
  let stderr = '';
  proc.stdout.on('data', (d) => (stdout += d));
  proc.stderr.on('data', (d) => (stderr += d));
  const done = new Promise((resolveClose) => proc.on('close', resolveClose));

  const view = await waitFor('cli login user code', async () => {
    const line = stdout.split('\n').find((l) => l.trim().startsWith('{'));
    if (!line) return null;
    try {
      const parsed = JSON.parse(line);
      return parsed.user_code ? parsed : null;
    } catch {
      return null;
    }
  });
  return { proc, done, getStdout: () => stdout, getStderr: () => stderr, view };
}

async function requestMagicLink(email) {
  const res = await fetch(`${BASE}/api/auth/request-link`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ email }),
  });
  if (!res.ok) fail(`request-link for ${email} returned ${res.status}`);
}

async function magicLinkFrom(logs) {
  // Take the LAST logged link: earlier magic links stay in the log.
  return waitFor('magic link in server logs', async () => {
    const matches = logs().match(/https?:\/\/\S+\/api\/auth\/verify\?token=\S+/g);
    return matches ? matches.at(-1) : null;
  });
}

// Approve a pending device request on the real server-rendered page and
// check that the page shows exactly the expected client + scopes.
async function approveOnPage(page, url, clientName, scopes) {
  await page.goto(url, { waitUntil: 'networkidle0' });
  const body = await page.waitForFunction(
    () => document.querySelector('form[action="/oauth/device/approve"]') !== null,
    { timeout: 10_000 },
  ).then(() => page.evaluate(() => document.body.innerText));
  if (!body.includes(clientName)) {
    fail(`approval page does not mention "${clientName}": ${JSON.stringify(body)}`);
  }
  for (const scope of scopes) {
    if (!body.includes(scope)) {
      fail(`approval page for ${clientName} is missing scope ${scope}: ${JSON.stringify(body)}`);
    }
  }
  await page.click('form[action="/oauth/device/approve"] button[type="submit"]');
  await page.waitForFunction(() => location.pathname === '/oauth/device/done', { timeout: 10_000 });
  const doneText = await page.evaluate(() => document.body.innerText);
  if (!doneText.includes('approved')) fail(`done page says: ${JSON.stringify(doneText)}`);
}

// Poll the token endpoint until a device grant is issued.
async function pollForGrant(deviceCode) {
  const body = new URLSearchParams({
    grant_type: 'urn:ietf:params:oauth:grant-type:device_code',
    device_code: deviceCode,
  });
  return waitFor('device grant', async () => {
    const res = await fetch(`${BASE}/api/oauth/token`, {
      method: 'POST',
      headers: { 'content-type': FORM },
      body,
    });
    if (res.status === 200) return res.json();
    return null; // authorization_pending / slow_down — keep polling
  }, 30_000, 1_000);
}

// A full stdio MCP session: initialize, tools/list, then tool calls.
async function mcpRoundTrip(tokenFile) {
  const proc = spawn(CLI, ['mcp', '--server', BASE, '--token-file', tokenFile], {
    cwd: ROOT,
    stdio: ['pipe', 'pipe', 'pipe'],
    env: { ...process.env, HUDDLE_CLI_TOKEN_FILE: tokenFile },
  });
  const responses = new Map();
  let buf = '';
  proc.stdout.on('data', (d) => {
    buf += d;
    let idx;
    while ((idx = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, idx).trim();
      buf = buf.slice(idx + 1);
      if (line) {
        try {
          const parsed = JSON.parse(line);
          responses.set(parsed.id, parsed);
        } catch {
          // Non-JSON noise on stdout would itself be a bug worth failing
          // on — but let the assertions below catch missing responses.
        }
      }
    }
  });
  let mcpErr = '';
  proc.stderr.on('data', (d) => (mcpErr += d));
  const send = (id, method, params) =>
    proc.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);

  try {
    send(1, 'initialize', {
      protocolVersion: '2024-11-05',
      capabilities: {},
      clientInfo: { name: 'huddle-e2e', version: '0' },
    });
    send(2, 'tools/list');
    send(3, 'tools/call', {
      name: 'create_meeting',
      arguments: { title: 'MCP standup', start: '1h', duration_minutes: 30 },
    });

    const init = await waitFor('mcp initialize', () => responses.get(1));
    if (init.result?.protocolVersion !== '2024-11-05') {
      fail(`mcp initialize answered ${JSON.stringify(init)}`);
    }
    const tools = await waitFor('mcp tools/list', () => responses.get(2));
    const names = (tools.result?.tools ?? []).map((t) => t.name).sort();
    if (JSON.stringify(names) !== JSON.stringify(['cancel_meeting', 'create_meeting', 'get_meeting', 'list_upcoming'])) {
      fail(`mcp tools/list returned ${JSON.stringify(names)}`);
    }

    const created = await waitFor('mcp create_meeting', () => responses.get(3));
    if (created.result?.isError) fail(`mcp create_meeting failed: ${JSON.stringify(created)}`);
    const joinUrl = created.result?.structuredContent?.join_url ?? '';
    const code = created.result?.structuredContent?.meeting?.meeting?.code ?? '';
    if (!code || !joinUrl.endsWith(`/m/${code}`)) {
      fail(`mcp create_meeting gave ${JSON.stringify(created.result)}`);
    }

    send(4, 'tools/call', { name: 'list_upcoming' });
    const listed = await waitFor('mcp list_upcoming', () => responses.get(4));
    const listedCodes = (listed.result?.structuredContent?.meetings ?? [])
      .map((m) => m.meeting?.meeting?.code);
    if (!listedCodes.includes(code)) {
      fail(`mcp list_upcoming does not contain the fresh meeting: ${JSON.stringify(listed.result)}`);
    }

    send(5, 'tools/call', { name: 'cancel_meeting', arguments: { code } });
    const cancelled = await waitFor('mcp cancel_meeting', () => responses.get(5));
    if (cancelled.result?.structuredContent?.cancelled !== true) {
      fail(`mcp cancel_meeting answered ${JSON.stringify(cancelled)}`);
    }
  } finally {
    proc.stdin.end();
    proc.kill('SIGTERM');
  }
  return { mcpErr };
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

const browser = await puppeteer.launch({
  executablePath: CHROME,
  headless: true,
  args: ['--no-sandbox', '--disable-gpu', '--disable-dev-shm-usage'],
});
const server = await bootServer();
const tokenFile = join(mkdtempSync(join(tmpdir(), 'huddle-cli-tokens-')), 'cli.json');
let loginHandle = null;

try {
  // Sign the browser in with a magic link so /oauth/device sees a session.
  await requestMagicLink(EMAIL);
  const link = await magicLinkFrom(server.logs);
  const page = await browser.newPage();
  page.on('pageerror', (error) => console.log(`[pageerror] ${error.message}`));
  await page.goto(link, { waitUntil: 'networkidle0' });
  await page.waitForSelector('[data-testid="create-btn"]', { timeout: 15_000 });
  console.log('browser signed in via magic link');

  // --- Device flow login ----------------------------------------------
  loginHandle = await startCliLogin(tokenFile);
  console.log(`cli printed user code ${loginHandle.view.user_code}`);
  const verifyUrl =
    loginHandle.view.verification_uri_complete ?? `${loginHandle.view.verification_uri}?user_code=${encodeURIComponent(loginHandle.view.user_code)}`;
  await approveOnPage(
    page,
    verifyUrl,
    'huddle-cli',
    ['meetings:read', 'meetings:write'],
  );
  const loginCode = await loginHandle.done;
  const loginStdout = loginHandle.getStdout();
  loginHandle = null;
  if (loginCode !== 0) fail(`huddle-cli login exited ${loginCode}`);
  const lastJson = loginStdout
    .trim()
    .split('\n')
    .filter((l) => l.trim().startsWith('{'))
    .at(-1);
  if (JSON.parse(lastJson).email !== EMAIL) {
    fail(`huddle-cli login reported ${lastJson}, want email ${EMAIL}`);
  }
  if (!existsSync(tokenFile)) fail('huddle-cli login stored no token file');
  console.log('huddle-cli login completed the device flow and stored a login');

  // --- CLI CRUD round trip --------------------------------------------
  const created = await runCli(
    ['create', '--title', 'Agent sync', '--in', '1h', '--json'],
    tokenFile,
  );
  if (created.code !== 0) fail(`cli create exited ${created.code}: ${created.stderr}`);
  const createdJson = JSON.parse(created.stdout);
  const code = createdJson.meeting.meeting.code;
  if (createdJson.meeting.meeting.title !== 'Agent sync') {
    fail(`cli create returned ${created.stdout}`);
  }
  if (!createdJson.join_url.endsWith(`/m/${code}`)) fail(`cli create join url ${createdJson.join_url}`);
  console.log(`cli created meeting ${code} and printed its join link`);

  const listed = await runCli(['list', '--json'], tokenFile);
  if (listed.code !== 0) fail(`cli list exited ${listed.code}: ${listed.stderr}`);
  if (!listed.stdout.includes(code)) fail(`cli list does not contain ${code}`);
  console.log('cli list sees the meeting');

  const got = await runCli(['get', code, '--json'], tokenFile);
  if (got.code !== 0 || !got.stdout.includes(code)) fail(`cli get failed: ${got.stderr}`);
  const ics = await runCli(['ics', code, '--json'], tokenFile);
  if (ics.code !== 0 || !JSON.parse(ics.stdout).ics.includes('BEGIN:VCALENDAR')) {
    fail(`cli ics failed: ${ics.stdout}${ics.stderr}`);
  }
  console.log('cli get and ics exports work');

  const cancelled = await runCli(['cancel', code, '--yes', '--json'], tokenFile);
  if (cancelled.code !== 0 || JSON.parse(cancelled.stdout).cancelled !== true) {
    fail(`cli cancel failed: ${cancelled.stdout}${cancelled.stderr}`);
  }
  console.log('cli cancel works');

  // --- MCP over stdio ---------------------------------------------------
  const { mcpErr } = await mcpRoundTrip(tokenFile);
  if (mcpErr.trim()) console.log(`[mcp stderr] ${mcpErr.trim().split('\n').at(-1)}`);
  console.log('huddle-cli mcp served initialize/tools-list/tool-calls');

  // --- Read-only approval cannot write ---------------------------------
  const register = await fetch(`${BASE}/api/oauth/device/register`, {
    method: 'POST',
    headers: { 'content-type': FORM },
    body: new URLSearchParams({ client_name: 'reader-script', scope: 'meetings:read' }),
  });
  if (register.status !== 200) fail(`read-only register returned ${register.status}`);
  const device = await register.json();
  const readerUrl =
    device.verification_uri_complete ?? `${device.verification_uri}?user_code=${encodeURIComponent(device.user_code)}`;
  // The approval page for a read-only request must not offer meetings:write.
  await approveOnPage(page, readerUrl, 'reader-script', ['meetings:read']);
  const readerPageText = await page.evaluate(() => document.body.innerText);
  if (readerPageText.includes('meetings:write')) {
    fail('read-only approval page offered meetings:write');
  }
  const grant = await pollForGrant(device.device_code);
  if (grant.scope !== 'meetings:read') fail(`read-only grant scope ${grant.scope}`);

  const readOnlyCreate = await fetch(`${BASE}/api/meetings`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      authorization: `Bearer ${grant.access_token}`,
    },
    body: JSON.stringify({
      title: 'Should not exist',
      starts_at: new Date(Date.now() + 3_600_000).toISOString().replace(/\.\d+Z$/, 'Z'),
      ends_at: new Date(Date.now() + 7_200_000).toISOString().replace(/\.\d+Z$/, 'Z'),
    }),
  });
  if (readOnlyCreate.status !== 403) {
    fail(`read-only bearer create returned ${readOnlyCreate.status}, want 403`);
  }
  console.log('read-only token is enforced server-side (create → 403)');

  // --- Dashboard: authorizations list + revoke --------------------------
  await page.goto(`${BASE}/meetings`, { waitUntil: 'networkidle0' });
  await page.waitForSelector('[data-testid="authorizations-list"] [data-testid^="authorization-row-"]', { timeout: 15_000 });
  const authorizations = await page.evaluate(async () => (await fetch('/api/oauth/authorizations')).json());
  const cliAuth = authorizations.find((a) => a.client_name === 'huddle-cli');
  if (!cliAuth) fail(`dashboard authorizations lack huddle-cli: ${JSON.stringify(authorizations)}`);

  await page.click(`[data-testid="revoke-auth-${cliAuth.id}"]`);
  await page.waitForFunction(
    (id) => !document.querySelector(`[data-testid="revoke-auth-${id}"]`),
    { timeout: 15_000 },
    cliAuth.id,
  );
  const remaining = await page.evaluate(async () => (await fetch('/api/oauth/authorizations')).json());
  if (remaining.some((a) => a.id === cliAuth.id)) {
    fail('revoked authorization is still listed');
  }
  console.log('dashboard lists authorizations and revoke removes the row');

  // Revocation kills the refresh token: drop the cached (still-valid,
  // stateless) access token so the next CLI run must refresh — and fail.
  const stored = JSON.parse(readFileSync(tokenFile, 'utf8'));
  stored.access_token = null;
  stored.access_expires_at = null;
  writeFileSync(tokenFile, JSON.stringify(stored));
  const afterRevoke = await runCli(['list', '--json'], tokenFile);
  if (afterRevoke.code !== 2) {
    fail(`cli after revocation exited ${afterRevoke.code}, want 2 (auth): ${afterRevoke.stdout}${afterRevoke.stderr}`);
  }
  console.log('revoked refresh token rejects the CLI (exit 2)');

  console.log('PASS: agent access e2e green');
} catch (error) {
  if (!(error instanceof FailSignal)) {
    console.error('FAIL: unexpected error');
    console.error(error);
    process.exitCode = 1;
  }
} finally {
  loginHandle?.proc.kill('SIGTERM');
  await browser.close().catch(() => {});
  server.cleanup();
  rmSync(join(tokenFile, '..'), { recursive: true, force: true });
  if (process.env.DUMP_SERVER_LOGS !== '0') {
    console.log('--- server logs (tail) ---');
    console.log(server.logs().slice(-4000));
  }
}

process.exit(process.exitCode ?? 0);
