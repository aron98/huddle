// Settings persistence for the desktop shell.
//
// The only setting is `serverUrl`: the origin of the Huddle server whose
// built-in web client (served same-origin, per ADR 004) the shell loads.
// `fs` is injected so the pure helpers are testable without Electron.

import path from "node:path";

const LOCAL_HOSTS = new Set(["localhost", "127.0.0.1", "[::1]", "::1"]);

export function settingsPath(userDataDir) {
  return path.join(userDataDir, "settings.json");
}

/**
 * Normalizes a user-entered server address to a bare origin URL.
 *
 * Accepted: "localhost:3000", "http://localhost:3000", "meet.example.com",
 * "https://meet.example.com". Scheme-less input defaults to http for
 * loopback hosts and https for everything else. Any path, query, hash or
 * userinfo is rejected: the web client is served at the server root and
 * speaks only relative-path REST + same-origin WS.
 *
 * @returns {{ ok: true, url: string } | { ok: false, error: string }}
 */
export function normalizeServerUrl(raw) {
  if (typeof raw !== "string") return { ok: false, error: "Server address is required." };
  let input = raw.trim();
  if (!input) return { ok: false, error: "Server address is required." };

  // No scheme given: pick one from the first host segment before adding it,
  // so "localhost:3000" is not mistaken for a "localhost:" protocol.
  if (!/^[A-Za-z][A-Za-z0-9+.-]*:\/\//.test(input)) {
    const host = input.split("/")[0].split(":")[0].toLowerCase();
    input = (LOCAL_HOSTS.has(host) ? "http://" : "https://") + input;
  }

  let url;
  try {
    url = new URL(input);
  } catch {
    return { ok: false, error: "That does not look like a server address." };
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") {
    return { ok: false, error: "Only http and https servers are supported." };
  }
  if (url.username || url.password) {
    return { ok: false, error: "Userinfo is not allowed in the server address." };
  }
  if (!url.hostname) {
    return { ok: false, error: "The address is missing a host." };
  }
  if (url.pathname !== "/" || url.search || url.hash) {
    return {
      ok: false,
      error: "Enter just the server origin — the Huddle web client is served at the root.",
    };
  }
  return { ok: true, url: url.origin };
}

/**
 * @returns {{ serverUrl: string | null }} missing/corrupt files read as unset
 */
export function loadSettings(fs, file) {
  try {
    const data = JSON.parse(fs.readFileSync(file, "utf8"));
    const serverUrl = typeof data?.serverUrl === "string" ? data.serverUrl : null;
    return { serverUrl };
  } catch {
    return { serverUrl: null };
  }
}

export function saveSettings(fs, file, settings) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(settings, null, 2) + "\n", "utf8");
}
