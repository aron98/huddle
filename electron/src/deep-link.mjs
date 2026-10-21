// Pure parser for `huddle://` deep links.
//
// A meeting link looks like `huddle://<host>/<path>` — e.g.
// `huddle://meet.example.com/j/abc123` — and means "open the Huddle client
// of the server at <host> at <path>". Scheme selection: loopback hosts get
// http, everything else https, and when the link host matches the already
// configured server the configured scheme is reused (so a server that
// really is plain http on a LAN hostname keeps working).
//
// The web client is same-origin only, so the deep link carries just enough
// to reconstruct the page URL: { serverUrl, path }.

const PREFIX = "huddle://";

/**
 * @param {string} link raw deep-link URL
 * @param {string | null} configuredOrigin the saved server origin, if any
 * @returns {{ serverUrl: string, path: string } | null}
 */
export function parseDeepLink(link, configuredOrigin) {
  if (typeof link !== "string" || !link.startsWith(PREFIX)) return null;
  const rest = link.slice(PREFIX.length);
  if (!rest) return null;

  const slash = rest.indexOf("/");
  const hostPart = slash === -1 ? rest : rest.slice(0, slash);
  const pathPart = slash === -1 ? "/" : rest.slice(slash);
  if (!hostPart) return null;

  // Validate the authority by parsing it under a throwaway scheme; this
  // rejects embedded spaces, stray credentials and other junk.
  let probe;
  try {
    probe = new URL(`http://${hostPart}`);
  } catch {
    return null;
  }
  if (probe.username || probe.password || probe.search || probe.hash) return null;
  if (!probe.hostname) return null;

  const isLoopback =
    probe.hostname === "localhost" ||
    probe.hostname === "127.0.0.1" ||
    probe.hostname === "[::1]";
  let scheme = isLoopback ? "http" : "https";

  if (configuredOrigin) {
    try {
      const configured = new URL(configuredOrigin);
      if (configured.hostname === probe.hostname) {
        const reused = configured.protocol.replace(/:$/, "");
        if (reused === "http" || reused === "https") scheme = reused;
      }
    } catch {
      // Malformed stored setting: fall through to the default rules.
    }
  }

  return { serverUrl: `${scheme}://${probe.host}`, path: pathPart };
}
