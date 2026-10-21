# ADR 008: TLS termination inside the binary

- Status: accepted
- Date: 2026-10-21
- Relates to: [ADR 004](004-single-binary-embedded-ui.md), issue #30, issue #31

## Context

Until now the serving model was: huddle speaks plain HTTP on `HUDDLE_BIND`
and something else — Caddy, Nginx, an ALB, a k8s ingress — terminates TLS on
443 in front of it. That decision was never its own ADR; it lived as prose:

- the architecture goal "Self-hostable with minimal ops: binary + SQLite
  file + coturn + **TLS proxy**" (`docs/ARCHITECTURE.md`),
- the deployment-target row "Own VPS (primary): Single binary + SQLite +
  coturn + **Caddy/TLS**",
- the firewall table pinning 443 TCP to "HTTPS + WebSocket signaling" that a
  proxy would own, and
- ADR 004's positive consequence that the single binary needs "no Nginx,
  no Node, no second container" — for the *application* tier, while the
  TLS tier quietly still required exactly such a second component.

That contradicts the core promise of ADR 004 (`scp huddle user@vps &&
./huddle` is the whole install) and pushes every self-hoster to learn and
operate a proxy for what is, in 2026, a solved library problem in Rust:
ACME issuance (Let's Encrypt) and TLS 1.2/1.3 termination via rustls.

> **Note on numbering.** Issue #30 frames this as "reverses ADR 005".
> That reference is wrong: ADR 005 records the relicensing to Apache-2.0
> and says nothing about TLS or proxies. The decision actually being
> reversed was the implicit reverse-proxy arrangement documented only in
> ARCHITECTURE.md prose and ADR 004's "no Nginx" consequence, as quoted
> above. This ADR supersedes that arrangement.

## Decision

- **The binary terminates TLS itself.** `huddle` binds HTTPS/WSS directly
  via `axum-server` with rustls; ACME (Let's Encrypt, TLS-ALPN-01 on the
  serving port) is handled in-process by `rustls-acme`, with certificate
  and account state cached under `data_dir/acme-cache`. No proxy, no
  second container, no certbot cron.
- **Three config modes, one surface.** A `[tls]` section in `huddle.toml`
  mirrors the `[mail]` selection pattern (ADR 006):
  - `mode = "acme"` — `[tls.acme]` with `domains`, optional `contact`
    (`mailto:` prepended if missing), `staging` flag (Let's Encrypt
    staging directory for testing issuance);
  - `mode = "static"` — `[tls.certs]` with `cert` and `key` PEM paths
    (for certs from any other source);
  - no `[tls]` section — plain HTTP, intended for local development and
    tests only (the server logs a warning at startup; e2e keeps running
    over HTTP on 127.0.0.1).
  An explicit `mode` wins; without one the mode is detected from whichever
  sub-section is present; ambiguity and half-configuration fail loudly at
  startup instead of guessing. Missing cert/key files fail fast at
  config load. `HUDDLE_TLS_MODE` / `HUDDLE_TLS_DOMAINS` / … env vars are
  applied explicitly after figment extraction (figment's flat env keys
  cannot reach nested sections — the `HUDDLE_OIDC_*`/`HUDDLE_MAIL_*`
  precedent), including figment value syntax for the domains list.
- **Security posture is config-driven, not string-driven.** When TLS is
  active — or `base_url` is already `https://` — session cookies are
  marked `Secure` (previously inferred from a `base_url.starts_with`
  check) and responses carry `Strict-Transport-Security` (HSTS) in TLS
  mode. Plain-HTTP localhost development and e2e are unchanged.
- **rustls safe defaults, not hand-rolled crypto policy.** We do not
  pick cipher suites or protocol versions ourselves; rustls's curated
  defaults (TLS 1.2 + 1.3, safe suite list) are used as shipped and
  documented here as a deliberate choice. The aws-lc-rs provider is
  pinned by installing it explicitly at startup, because the dependency
  graph (lettre/reqwest via ring, axum-server/rustls-acme via aws-lc-rs)
  would otherwise enable two providers and panic at runtime when rustls
  cannot determine the process-level one.
- **Reverse proxy demoted, not banned.** Running Caddy/Nginx/an ALB in
  front is still a documented option — the right one behind load
  balancers or on Kubernetes, where the ingress owns 443 anyway. It just
  stops being the *required* path for a single VPS.
- **TURN over TLS stays deployment config.** The binary's TLS work is for
  HTTP/WS; coturn's `tls-listening-port` needs its own certificate and is
  therefore a coturn config concern. We ship a production template at
  `deploy/turn/turnserver.conf` (see issue #31) instead of extending the
  server.

## Consequences

**Positive**

- ADR 004's "whole install" promise finally holds end to end: one binary,
  one UDP media port, one TCP serving port — including HTTPS and
  certificate lifecycle.
- ACME certificates renew automatically in-process; no certbot timers, no
  restart choreography, no proxy config to keep in sync with huddle.
- WSS and HTTPS keep the `ConnectInfo`-based per-IP rate limiting and
  `axum`'s `into_make_service_with_connect_info` semantics — the TLS layer
  is below the router, not beside it.
- A cheap real handshake test (rcgen self-signed cert + reqwest with
  `danger_accept_invalid_certs`) exercises the full static-TLS path in
  `cargo test`, no network required.

**Negative**

- TLS-ALPN-01 requires the server to own port 443 directly; the ACME
  challenge is answered on the same listener as production traffic, so
  first boot before issuance accepts no normal TLS handshakes (expected,
  brief).
- ACME state (account key, cached certs) adds files under `data_dir` that
  must survive restarts — already persistent in all supported deployment
  shapes.
- The aws-lc-rs provider pin is a direct `rustls` dependency added solely
  to resolve the dual-provider ambiguity; it must be revisited if the
  leaf dependency set changes providers.
- k8s/LB deployments still front huddle with something that owns 443;
  built-in TLS mainly serves the single-VPS and static-cert cases there.

## Amendment

- If a future need arises to run TLS on a port other than 443 with
  HTTP-01 challenges (which require serving tokens over plain HTTP),
  revisit the challenge type rather than adding a second listener.
