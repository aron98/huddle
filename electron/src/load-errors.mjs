// Friendly messages for Chromium net errors from a failed mainWindow
// loadURL (issue #107: a failed Connect used to be logged to the main-
// process console only; the user saw nothing). Pure module — no Electron
// imports — so node --test covers it like settings.mjs / deep-link.mjs.

const NET_ERROR_RE = /ERR_[A-Z0-9_]+/;

// Ordered only for readability; lookup is exact.
const HINTS = {
  ERR_CONNECTION_REFUSED:
    "Could not reach the server — it refused the connection. Check that the server is running at that address.",
  ERR_CONNECTION_TIMED_OUT:
    "Could not reach the server — the connection timed out.",
  ERR_TIMED_OUT: "Could not reach the server — the connection timed out.",
  ERR_ADDRESS_UNREACHABLE:
    "Could not reach the server — the address is unreachable from this machine.",
  ERR_NAME_NOT_RESOLVED:
    "Could not resolve that server address. Check the hostname for typos.",
  ERR_CONNECTION_CLOSED:
    "The server closed the connection before responding. Check that the address points at a Huddle server.",
  ERR_CONNECTION_RESET:
    "The connection to the server was reset. Check that the address points at a Huddle server.",
  ERR_EMPTY_RESPONSE:
    "The server sent an empty response. Check that the address points at a Huddle server.",
  ERR_CERT_AUTHORITY_INVALID:
    "The server's https certificate is not trusted (e.g. self-signed). The desktop shell needs a server with a valid certificate.",
  ERR_CERT_DATE_INVALID:
    "The server's https certificate has expired or is not valid yet.",
  ERR_CERT_COMMON_NAME_INVALID:
    "The server's https certificate does not match its hostname.",
  ERR_INSECURE_RESPONSE:
    "The server's https response was insecure (bad or untrusted certificate).",
  ERR_SSL_PROTOCOL_ERROR:
    "A TLS/SSL error occurred while connecting. If the server only speaks plain http, try http:// as the scheme.",
  ERR_SSL_VERSION_OR_CIPHER_MISMATCH:
    "The server rejected the TLS handshake. If the server only speaks plain http, try http:// as the scheme.",
};

/**
 * Turns a loadURL rejection into a user-facing sentence. The raw Chromium
 * net error code is always included for diagnosability. Accepts anything
 * rejection-shaped (Error with the code in the message or in err.code,
 * string, null) and never throws.
 *
 * @param {unknown} err
 * @returns {string}
 */
export function describeLoadError(err) {
  const source = typeof err === "string" ? err : err?.code ?? err?.message ?? "";
  const raw = typeof source === "string" ? source : String(source);
  const code = raw.match(NET_ERROR_RE)?.[0];
  if (code) {
    const hint = HINTS[code];
    return hint ? `${hint} (net error ${code})` : `Could not load the server page (net error ${code}).`;
  }
  const detail = raw.trim() || "unknown error";
  return `Could not load the server page (${detail}).`;
}
