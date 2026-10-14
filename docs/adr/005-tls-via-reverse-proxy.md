# ADR 005: TLS via reverse proxy, not built-in

- Status: accepted
- Date: 2026-10-14

## Context

The server speaks plain HTTP/WS on `HUDDLE_BIND` — media is always safe
(WebRTC mandates DTLS-SRTP), but signaling, the web UI, REST, and chat are
plaintext on the wire until something terminates TLS (issue #3). Two ways to
get HTTPS in production:

1. **Built-in TLS**: link `rustls`/`axum-server` (plus ACME via
   `rustls-acme` or a manual cert path) into the huddle binary itself.
2. **Reverse proxy**: terminate TLS in Caddy/nginx/a cloud LB in front of
   the unchanged binary.

## Decision

**Reverse proxy only.** huddle does not gain a built-in TLS mode; the
documented production path is a proxy (see the README's Self-hosting →
HTTPS section for copy-pasteable Caddy and nginx configs).

The single-binary ethos actually argues *for* the proxy: the promise is "one
huddle binary" — it is not "the whole TLS story in one process". Caddy is
itself a single binary with automatic certificates, so "scp two binaries"
still beats the JVM incumbents, and TLS termination stays where the
ecosystem has already solved it.

## Consequences

**Positive**
- huddle stays free of certificate lifecycle: ACME ordering, renewal, OCSP,
  cipher/protocol hardening, and HTTP→HTTPS redirects all live in the
  proxy or LB, which do this well and get security updates without a
  huddle re-release.
- No split of the session-cookie story: `Secure` already keys off
  `https://` in `base_url` (`crates/server/src/auth.rs`), which works
  identically behind any terminator.
- k8s and cloud users were going to run a termination layer anyway
  (ingress, ALB); the proxy path matches all deployment targets uniformly.

**Negative**
- One more moving part on the VPS path — mitigated by recommending Caddy,
  where "one extra binary" is the entire cost.
- The proxy must be WebSocket-aware (upgrade headers, no 60s idle cutoff)
  and must stay out of the media path (media is UDP and never proxied) —
  documented pitfalls in the README.
- Per-IP rate limiting sees the proxy's address, not the client's — known
  limitation, noted in the docs; X-Forwarded-For handling can be added
  later if it bites.

**Revisit if**: a credible "huddle alone on a box with zero companion
processes, TLS included" demand appears — the change would be an
`axum-server` + `rustls-acme` bind option, isolated behind config.
