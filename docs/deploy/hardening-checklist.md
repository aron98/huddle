# Huddle hardening checklist

Cross-platform security checklist for any huddle deployment — VPS
([ubuntu-vps.md](ubuntu-vps.md)), Docker Compose
([docker-compose.md](docker-compose.md)), or Kubernetes
([kubernetes.md](kubernetes.md)). Every claim below is backed by the server code
(`crates/server/src/`) or an ADR; file references point at the current tree.

Use it two ways: as a pre-go-live checklist, and as an audit script — most items
say *who* enforces them (the server by construction, or you via configuration).

## TLS, HSTS, and cookies

- [ ] **Terminate TLS somewhere.** Either huddle itself (built-in ACME or static
  PEM, ADR 008) or your proxy/ingress. huddle refuses `[tls]` with no `mode` when
  both sub-sections are present, and fails fast in `static` mode if the cert/key
  files are missing — misconfiguration is a startup error, not a runtime surprise.
- [ ] **HSTS is automatic only in TLS mode.** When huddle terminates TLS it sends
  `Strict-Transport-Security: max-age=31536000; includeSubDomains` on every
  response (verified by the `hsts_header_present_only_when_tls_is_active` test in
  `http.rs`). **Behind a proxy huddle never sends it** — set the header at the
  proxy/ingress instead.
- [ ] **`Secure` cookies are keyed off `base_url`.** `config.rs`:
  `secure_cookies() = tls.is_some() || base_url.starts_with("https://")`. Keep
  `base_url` on `https://…` even in proxy variants, and cookies get
  `; Secure` automatically. Cookies are always `HttpOnly; SameSite=Lax`.
- [ ] **ACME cache survives restarts.** Built-in ACME persists certificates and
  the account key under `<data_dir>/acme-cache`. Deleting it burns your Let's
  Encrypt rate limits and forces re-issuance.
- [ ] **TLS 1.2+1.3 only.** rustls curated defaults; aws-lc-rs provider. Nothing
  to configure — listed so you know downgrading is not an option.

## Secrets and sessions

- [ ] **Set a persistent `session_secret`.** Otherwise a random secret is
  generated per boot (with a warning log) and every restart invalidates every
  session. Generate with `openssl rand -hex 32`; in Kubernetes use the chart's
  `existingSecret` (key `HUDDLE_SESSION_SECRET`).
- [ ] **Understand what sessions are.** Session cookies are *stateless*:
  `v1.<organizer-id>.<email>.<unix-expiry>.<hmac>` — an HMAC-SHA256 over the full
  payload including expiry (`auth.rs`), parsed right-to-left because emails contain
  dots. There is **no sessions table**; nothing to revoke server-side except by
  rotating the secret. A leaked secret forges sessions until expiry
  (`session_ttl_min`, default 120) — treat it as a root credential.
- [ ] **TURN credentials are shared secrets.** They appear in `[[ice_servers]]`
  (huddle side) and coturn's `user=` line. Generate a strong random password; see
  the platform guides for the honest limits (huddle has no REST-auth credential
  helper today — static `lt-cred-mech` pairs only).

## Authentication and abuse control

- [ ] **Magic links are single-use, hashed at rest, short-lived.** Tokens are
  stored SHA-256-hashed (`login_tokens.hash`, `identity.rs`), claimed atomically,
  and expire after `login_token_ttl_min` (default 15). Even a full database read
  does not yield usable links. This is enforced by construction.
- [ ] **Rate limiting is on by default.** One per-IP sliding window (60 s) guards
  magic-link requests *and* new WebSocket connections: `rate_limit_per_min`
  (default 30; `0` disables). The limiter uses the socket peer address, so:
  - **Direct exposure (VPS/binary-owns-443):** works as designed.
  - **Behind a proxy/ingress: all clients share the proxy's IP budget.**
    Rate-limit at the proxy or raise/disable the server-side limit — but never
    leave both layers unconfigured. (No `X-Forwarded-For` handling exists.)
- [ ] **WebSocket messages are capped.** 256 KiB per frame *and* per message
  (`session.rs`); E2EE keying payloads are capped at 22 KiB base64. Nothing to
  configure.
- [ ] **Meeting links are capabilities.** Possession of a join link grants the
  meeting; passcodes are shared secrets compared in constant time. Don't put join
  URLs in publicly readable places; the e2e suites treat them as bearer tokens.

## Network exposure

- [ ] **Open only the port matrix.** 443/tcp (or proxy equivalent), 40000/udp
  SFU media, 3478/udp+tcp TURN, optionally 5349/tcp TURN-over-TLS (TCP only),
  49160–49200/udp TURN relay. Never expose the plain HTTP port (default
  `0.0.0.0:3000`) to the internet — that variant exists for loopback/proxy
  backends only.
- [ ] **TURN over TLS is TCP-only.** `turns:` implies `?transport=tcp`;
  coturn's DTLS rides the plain 3478/udp listener. There is no 5349/udp.
- [ ] **Gate `/metrics`.** The endpoint is unauthenticated unless
  `stats_token` is set (then it requires `Authorization: Bearer <token>`), and it
  discloses live meeting codes and participant display names — semi-sensitive.
  Set `stats_token` or firewall it to your monitoring network. `/api/health` is
  intentionally public and returns `{"status":"ok"}`.

## Logs and data at rest

- [ ] **Nothing secret is logged by construction.** Magic-link tokens are only
  ever stored hashed; there are no session bodies to log; the OIDC state cookie is
  scoped to `/api/auth` with a 600 s TTL. **Exception:** the `log` mail provider
  *prints full magic links* — use it only for development.
- [ ] **E2EE meetings leave nothing readable behind.** Media is
  client-side-encrypted (WebCrypto + SFrame, RFC 9605) — the SFU relays opaque
  ciphertext it cannot decrypt; chat bodies are stored as ciphertext (ADR 011);
  meeting titles are sealed to the organizer's identity key (`enc1.` prefix,
  ADR 012); keying envelopes are routed, never persisted. E2EE is a per-meeting
  organizer choice made at creation — nothing to configure server-side. E2EE and
  server-side recording are mutually exclusive (server refuses with
  `409 e2ee_active`).
- [ ] **Know the honest floor (ADR 012).** Even in E2EE meetings the operator can
  see: organizer email, roster and join/leave times, display names, join codes,
  passcodes, scheduling facts, traffic analysis (timing/sizes). Plaintext
  meetings additionally keep plaintext titles/chat/media. Size your trust
  accordingly.
- [ ] **Log rotation is delegated.** huddle logs to stdout/stderr only — journald
  (`SystemMaxUse=`), docker json-file `max-size`/`max-file`, or your k8s log
  pipeline owns retention. Set caps explicitly; there is no in-server rotation.
- [ ] **REST config hardening:** nothing listens on the admin plane — no admin
  API, no CLI socket (coturn is run with `no-cli` for the same reason).

## Backups

Back up `<data_dir>` (VPS/compose; the PVC on Kubernetes). It contains:

| Path | Contents | Restore without it |
|------|----------|---------------------|
| `<data_dir>/huddle.sqlite3` (+ `-wal`, `-shm`) | organizers, meetings, hashed login tokens | All accounts and schedules lost |
| `<data_dir>/acme-cache/` | Let's Encrypt certs + ACME account key | Forced re-issuance; rate-limit pain |
| `<data_dir>/recordings/` | In-progress + muxed recordings | Permanent loss |

- SQLite runs in WAL mode: back up with `sqlite3 huddle.sqlite3 '.backup …'`
  (or VACUUM INTO) rather than copying mid-write files, and test restores.
- Stop huddle or accept a crash-consistent copy. Recordings on the compose/k8s
  paths live on volumes — snapshot those.

## Upgrade path

1. Back up `<data_dir>` (above).
2. Swap the artifact: replace the binary (VPS), rebuild the image (compose), or
   `helm upgrade` with a pinned `image.tag` (k8s).
3. Start and watch the log: **schema migrations run automatically at startup**
   (`sqlx::migrate!` is embedded; there are no migration files to copy). There is
   **no downgrade path** — never roll back the binary past a migration.
4. Expect the restart to drop all in-flight meetings: SFU meeting state is
   in-memory. The k8s chart pins `strategy: Recreate` for exactly this reason —
   a rolling update is a rolling meeting-killer, not zero-downtime.

## Quick audit questions

1. Is TLS terminated (and by whom — huddle or proxy)? Does *that* layer send
   HSTS?
2. Is `base_url` `https://…`? (Cookies only get `Secure` when it is.)
3. Is `session_secret` persistent and 32+ bytes of entropy?
4. Is `stats_token` set or `/metrics` firewalled?
5. Are TURN credentials random, mirrored byte-for-byte, and (k8s) Secret-mounted
   on the coturn side?
6. Is the rate limiter's proxy caveat handled (limit at the proxy, or knowingly
   disabled server-side)?
7. Are only the five port-matrix entries reachable from the internet?
8. Is the `log` mail provider definitely not used in production?
9. When was `<data_dir>` last backed up — and was a restore ever tested?
10. If E2EE matters to your users: do they know it's per-meeting, opt-in at
    creation, and that the operator floor (roster, times, sizes) still exists?
