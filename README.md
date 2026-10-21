# huddle

[![ci](https://github.com/aron98/huddle/actions/workflows/ci.yml/badge.svg)](https://github.com/aron98/huddle/actions/workflows/ci.yml)

**Lightweight, self-hostable virtual meetings — in Rust.**

huddle is an open-source video meeting application you can run as a single binary
on your own server: one process, one SQLite file, one TURN companion. No JVM, no
Node farm, no 40-step install guide.

See [Development](#development) for the dev loop or
[Self-hosting](#self-hosting) for production deployment.

## Documentation

- [ARCHITECTURE](docs/ARCHITECTURE.md) — system design
- [Deployment guides](docs/deploy/) — Ubuntu VPS, Docker Compose, Kubernetes, hardening checklist
- [ADRs](docs/adr/) — the decisions we've made and why
- [Post-MVP milestone](https://github.com/aron98/huddle/milestone/1) — what's
  next

## Features

- **Multi-party video calls** — camera and microphone for every participant,
  routed through a hand-rolled SFU on
  [str0m](https://github.com/lookback-io/str0m): each client uploads its media
  once (linear, not mesh), late joiners get keyframes instead of frozen tiles,
  and muting stops forwarding at the server.
- **Screen sharing** — dedicated tiles, immune to camera mute, with the
  browser's own "stop sharing" handled.
- **Chat** — relayed and persisted per meeting; history replays on join and
  survives a reload.
- **A modern web client in Rust** — Leptos compiled to WASM (about 300 KB
  gzipped): a prejoin page with camera preview and device pickers, an adaptive
  tile grid, roster, speaking indicators, and an in-call chat sidebar.
- **Scheduled meetings** — magic-link login for organizers, a meeting
  dashboard, ICS export for calendars, and join windows; guests join with just
  a display name, no account.
- **Small and observable** — one static binary with the web UI baked in (~10 MB
  idle RSS), SQLite storage, built-in HTTPS with automatic Let's Encrypt
  certificates, a Prometheus `/metrics` endpoint, per-IP rate limiting, and
  TURN via coturn for locked-down networks.

**Project status:** the MVP is complete — see the
[post-MVP milestone](https://github.com/aron98/huddle/milestone/1) for what's
next (SSO, simulcast, passcodes, E2EE, recording, and more).

## Stack

| Layer | Choice |
|---|---|
| Media routing (SFU) | Hand-rolled on [`str0m`](https://github.com/lookback-io/str0m) — pure Rust, sans-io WebRTC |
| Server | Rust, `axum` + `tokio`, WebSockets, `sqlx` |
| TLS | Built in: rustls termination + ACME (Let's Encrypt) via `axum-server` / `rustls-acme` — no reverse proxy required ([ADR 008](docs/adr/008-built-in-tls-termination.md)) |
| Storage | SQLite (single file) |
| Web client | Rust → WASM with [`Leptos`](https://github.com/leptos-rs/leptos); browser-native WebRTC via `web-sys` |
| Desktop (later) | Electron shell loading the server's web client ([ADR 014](docs/adr/014-desktop-shell-electron.md)) |
| TURN | `coturn` (self-hosted companion) |
| Deploy | Single binary on a VPS · AWS EC2/Fargate · Kubernetes (Helm) |

## Why another meeting app?

The self-hosted landscape splits into two camps: heavyweight platforms
(Jitsi Meet, BigBlueButton) that are powerful but painful to run, and minimal
tools (Galène) that are easy to run but spartan. huddle aims for the empty
middle: **single-binary operational simplicity, modern UX, Rust performance.**

## Development

Prerequisites: [Rust](https://rustup.rs) 1.96 (pinned via `rust-toolchain.toml`),
the `wasm32-unknown-unknown` target, and [trunk](https://trunkrs.dev) 0.21.x
(a release binary is fine: grab it from the
[trunk releases](https://github.com/trunk-rs/trunk/releases) page).

```sh
# quality gates (what CI runs)
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo check -p huddle-web --target wasm32-unknown-unknown

# dev loop, two terminals:
(cd crates/web && trunk watch)          # rebuilds the web bundle on change
HUDDLE_WEB_DIR=crates/web/dist cargo run -p huddle-server   # serves it
```

Then create a meeting and open its page:

```sh
curl -s localhost:3000/api/meetings \
  -H 'content-type: application/json' \
  -d '{"title":"standup","starts_at":"'"$(date -u +%Y-%m-%dT%H:%M:%SZ)"'","ends_at":"'"$(date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ)"'"}' | jq .
# → open http://localhost:3000/m/<code from the response>
```

(On macOS, replace the `-d '+1 hour'` date invocation with
`date -u -v+60M +%Y-%m-%dT%H:%M:%SZ` — BSD date has different flags.)

Configuration is `HUDDLE_*` environment variables (optionally a `huddle.toml`):
`HUDDLE_BIND` (default `0.0.0.0:3000`), `HUDDLE_DATA_DIR` (default `./data`),
`HUDDLE_MEDIA_PORT` (default `40000`), `HUDDLE_WEB_DIR` (serve a web bundle
from disk instead of the embedded one), and `HUDDLE_PUBLIC_IP` — the address
browsers send media to. Leave `HUDDLE_PUBLIC_IP` unset and the server
auto-detects its LAN address at startup, which is what local development
wants; production deployments should set it explicitly (e.g. the VPS's
public address).

One caveat the auto-detection exists to solve: advertising loopback only
works when the client's browser sources loopback traffic from loopback —
Linux Chrome does, macOS Chrome does not (it sends from the default
interface, and the mismatched reply source fails ICE validation). With
auto-detection you do not need to care about this on any platform.

Deployment (Docker, compose, Helm, release tarballs, coturn, firewall
ports) is covered in [Self-hosting](#self-hosting) below, along with optional
[Single sign-on (OIDC)](#single-sign-on-oidc) for the login flow.

## Observability

`GET /metrics` exposes live server state in the [Prometheus text
format](https://prometheus.io/docs/instrumenting/exposition_formats/) — one
scrape pulls everything a self-hoster needs:

| Family | Kind | Meaning |
|---|---|---|
| `huddle_rooms` | gauge | live meeting rooms |
| `huddle_room_participants{room}` | gauge | participants per room |
| `huddle_peer_rx_bytes_total` / `huddle_peer_tx_bytes_total{room,peer,id}` | counter | media payload bytes per peer |
| `huddle_peer_rx_frames_total` / `huddle_peer_tx_frames_total` | counter | media frames per peer |
| `huddle_peer_keyframes_requested_total` | counter | FIR/PLI keyframe requests per peer |
| `huddle_peer_bitrate_in_bps` / `huddle_peer_bitrate_out_bps` | gauge | last-window bitrates |
| `huddle_peer_rtt_ms` | gauge | round-trip time (sample appears once measured) |
| `huddle_peer_ingress_loss_fraction` / `huddle_peer_egress_loss_fraction` | gauge | packet loss per direction |

Optional gauges are omitted until they have been measured — a missing sample
means "not known yet", not zero.

**Access control.** The endpoint is unauthenticated by default, which matches
how Prometheus scrapes (it cannot log in or carry session cookies). Expect to
gate it at the deployment layer instead: a firewall rule, a reverse proxy in
front of the app port, or — the typical docker/k8s setup — a
Prometheus/Grafana sidecar that is the only thing with network access to the
server. If your topology needs app-layer auth, set `HUDDLE_STATS_TOKEN`; every
scrape must then send `Authorization: Bearer <token>` (constant-time
compared). Note that the endpoint discloses live room codes and participant
names, so treat it as semi-sensitive.

```yaml
# prometheus.yml
scrape_configs:
  - job_name: huddle
    # bearer_token: <HUDDLE_STATS_TOKEN>   # only if the token is set
    static_configs:
      - targets: ["huddle.internal:3000"]
```

## Self-hosting

Step-by-step production guides: [Ubuntu VPS](docs/deploy/ubuntu-vps.md) (static
binary + systemd + coturn), [Docker Compose](docs/deploy/docker-compose.md),
[Kubernetes](docs/deploy/kubernetes.md), and a cross-platform
[hardening checklist](docs/deploy/hardening-checklist.md). This section is the
configuration reference those guides link back to.

### Ports and firewall

A stock huddle needs exactly three holes in the firewall:

| Port | Protocol | Purpose |
|---|---|---|
| 3000 (or 443, see [TLS](#tls)) | TCP | web UI, REST API, and websocket signaling (`HUDDLE_BIND`) |
| 40000 | UDP | SFU media socket (`HUDDLE_MEDIA_PORT`) — one port for all calls |
| 3478 + 49160–49200 | UDP | coturn: TURN listener + relay port range (optional but recommended) |

The two huddle ports must be open to every participant's browser; the coturn
range only matters when you run the TURN companion (below). A browser that
cannot reach the UDP media port directly will fall back to TURN — which is
why coturn matters even for small self-hosted setups behind NAT.

### TLS

The binary terminates TLS itself ([ADR 008](docs/adr/008-built-in-tls-termination.md))
— no Caddy/Nginx/certbot layer required. Two modes, selected through the
`[tls]` section of `huddle.toml` (or `HUDDLE_TLS_*` env vars):

**ACME (recommended on a VPS).** Automatic Let's Encrypt certificates over
TLS-ALPN-01 — the challenge is answered on the serving port itself, so bind
443 and let the domain's A/AAAA record point at the box:

```toml
[tls]
mode = "acme"

[tls.acme]
domains = ["meet.example.com"]
contact = "admin@example.com"     # optional; mailto: is prepended if missing
# staging = true                   # issue against the LE staging CA while testing
```

Certificate and account state live in `<data_dir>/acme-cache` and renew
automatically in-process. Use `staging = true` while testing issuance to
avoid burning Let's Encrypt rate limits.

**Static certificates.** For certs from any other source (an existing
wildcard, an internal CA), point at PEM files:

```toml
[tls]
mode = "static"

[tls.certs]
cert = "/path/to/fullchain.pem"
key = "/path/to/privkey.pem"
```

Selection follows the `[mail]` pattern: an explicit `mode` always wins;
without one it is detected from whichever sub-section is present; ambiguous
or half-configured setups fail loudly at startup. Missing cert/key files
fail fast at config load.

Without a `[tls]` section huddle serves plain HTTP — that is meant for local
development and tests only, and the server logs a warning saying so. In TLS
mode responses carry `Strict-Transport-Security` and session cookies are
marked `Secure` (plain-HTTP localhost development keeps working either way).

The same settings work as environment variables — useful in containers:
`HUDDLE_TLS_MODE` (`acme`/`static`), and for ACME `HUDDLE_TLS_DOMAINS`
(figment value syntax: `'["meet.example.com"]'`), `HUDDLE_TLS_CONTACT`,
`HUDDLE_TLS_STAGING`; for static certs `HUDDLE_TLS_CERT` / `HUDDLE_TLS_KEY`.

A reverse proxy in front is still a fully supported option — it is the
natural shape behind a load balancer or on Kubernetes, where the ingress
owns 443 — it just is no longer *required* to get HTTPS on a single VPS.

### coturn (TURN relay)

[coturn](https://github.com/coturn/coturn) gives browsers a relay when their
own network blocks direct UDP (corporate firewalls, CGNAT, hotel wifi). A
minimal `turnserver.conf`:

```conf
listening-port=3478
fingerprint
lt-cred-mech
realm=meet.example.com
user=huddle:<a-long-random-password>
min-port=49160
max-port=49200
no-cli
# TLS: uncomment once you have certs (letsencrypt or otherwise)
# listening-ip= and relay-ip= if the box has several interfaces
```

Then tell huddle to advertise it (env var or `huddle.toml`):

```toml
# huddle.toml
[[ice_servers]]
urls = ["turn:meet.example.com:3478"]
username = "huddle"
credential = "<the same long random password>"
```

Browsers get the relay in their `Welcome` and use it only when direct
connectivity fails. For a quick check of the whole chain, the web client
supports `?turn=relay` — the call then refuses every non-relayed candidate,
which is exactly what the TURN e2e in CI asserts against a real local
coturn.

Production checklist: real credentials (not the dev pair), TLS on 5349 for
clients that split-tunnel TCP, and — unless you are building an internal
setup — do **not** copy `allow-loopback-peers` from the e2e configs; coturn
refuses loopback peers by default for good reason. A ready-to-edit
production `turnserver.conf` with the TLS listener enabled ships at
[`deploy/turn/turnserver.conf`](deploy/turn/turnserver.conf); the snippet
above (mirroring `deploy/coturn/turnserver.conf`) is the plain-UDP dev
setup.

### Email delivery

Organizers sign in with magic links, and huddle needs a way to deliver
them. Three transports are built in ([ADR 006](docs/adr/006-pluggable-mailer-transports.md));
with nothing configured, huddle **logs the link at `info` level instead of
sending it**, so evaluating a self-hosted instance never requires an email
account — copy the link out of the server log and paste it into a browser.

**`log` (default).** No configuration needed. Nothing leaves the machine;
the sign-in link is only in the server log.

**`smtp`** — send through a STARTTLS relay (lettre), configured by the
`[smtp]` section:

```toml
[smtp]
host = "smtp.example.com"
port = 587            # default 587
username = "huddle@example.com"
password = "…"
from = "huddle@example.com"   # default huddle@localhost
```

**`resend`** — deliver via the [Resend](https://resend.com) HTTPS API: no
SMTP relay to operate, just an API key. Select it with the `[mail]`
section:

```toml
[mail]
provider = "resend"

[mail.resend]
api_key = "re_…"              # from the Resend dashboard
from = "Huddle <huddle@example.com>"
```

Selection rules: an explicit `mail.provider` (`"log"`, `"smtp"`,
`"resend"`) always wins — `provider = "smtp"` uses the `[smtp]` section.
Without one, huddle detects the transport from whichever section is
present: `[mail.resend]` selects Resend, a bare `[smtp]` selects SMTP
(**a pre-existing SMTP-only config keeps working unchanged**), nothing at
all logs the link. Having both `[smtp]` and `[mail.resend]` without an
explicit `provider` is a startup error rather than a silent pick.

Everything is configurable via environment variables too — useful in
containers where a config file is awkward: `HUDDLE_MAIL_PROVIDER`
(`log`/`smtp`/`resend`), `HUDDLE_MAIL_RESEND_API_KEY`,
`HUDDLE_MAIL_RESEND_FROM`, and `HUDDLE_SMTP_HOST` / `HUDDLE_SMTP_PORT` /
`HUDDLE_SMTP_USERNAME` / `HUDDLE_SMTP_PASSWORD` / `HUDDLE_SMTP_FROM`. A
half-configured transport fails loudly at startup instead of silently
breaking sign-in.

### Single sign-on (OIDC)

Huddle signs organizers in with passwordless magic links by default. To
delegate authentication to an external identity provider instead, configure
OIDC — either `HUDDLE_OIDC_*` environment variables or an `[oidc]` section in
`huddle.toml`:

```toml
[oidc]
issuer_url = "https://sso.example.com/realms/main"
client_id = "huddle"
client_secret = "…"
```

The issuer must publish OpenID Connect discovery at
`<issuer_url>/.well-known/openid-configuration`; huddle reads the
authorization, token, and JWKS endpoints from it. Register the redirect URI
`<HUDDLE_BASE_URL>/api/auth/oidc/callback` with the provider.

While OIDC is configured, `GET /api/auth/oidc/login` starts the redirect flow
and `GET /api/auth/oidc/callback` completes it, landing the user on `/meetings`
with the same signed session cookie a magic link issues — organizers are keyed
by email, so the two flows share accounts and are interchangeable per user.
Magic-link login is replaced while SSO is active (`POST /api/auth/request-link`
answers `409` with the SSO login URL); sessions themselves are
provider-independent, so `/api/auth/me` keeps working the same way. Unset the
OIDC config and magic links return as the default flow.

Two things to check on the provider side: the ID token must carry an `email`
claim with `email_verified: true` (huddle is strict about this, since organizer
identity is the email), and the provider must support the authorization-code
flow — huddle always sends a PKCE `S256` challenge and a `nonce`, and only
accepts asymmetric signature algorithms (RS/ES/PS families) from its JWKS.

### Docker / Kubernetes

Docker: `docker build -t huddle .` produces the single-binary image. For
production use the [secure-by-default compose stack](deploy/compose/) (huddle +
coturn, guide: [docs/deploy/docker-compose.md](docs/deploy/docker-compose.md));
the root `docker-compose.yml` is a hot-reloading *dev* stack.

Kubernetes: the [Helm chart](deploy/helm/huddle) deploys one pod with the web
client embedded in the binary. It defaults to host networking because a k8s
Service cannot proxy the SFU's UDP media correctly for ICE — the advertised
candidate must be directly reachable. `helm install huddle deploy/helm/huddle
--set server.baseUrl=https://meet.example.com` and check `helm show values`
for the knobs (`server.publicIp`, `server.iceServers`, an existing session
secret, persistence). Full walkthrough incl. cert-manager/ingress and a coturn
DaemonSet: [docs/deploy/kubernetes.md](docs/deploy/kubernetes.md).

On k8s the ingress/LB usually owns 443 and the pod serves plain HTTP behind
it. Pod-terminated TLS (single-node, hostNetwork) is also possible — with ACME
the pod must own port 443 directly, which hostNetwork makes possible — but the
chart does not yet expose an extra-env/config knob to pass `HUDDLE_TLS_*`
through, so that variant needs a small chart extension (documented in the
[kubernetes guide](docs/deploy/kubernetes.md#pod-terminated-tls-variant-chart-extension-required)).
For TURN over TLS on the sidecar/host coturn, see the
[`deploy/turn/turnserver.conf`](deploy/turn/turnserver.conf) template.

Releases: pushing a `v*` tag makes CI build a static musl binary with the web
client baked in and attach `huddle-<version>-x86_64-unknown-linux-musl.tar.gz`
(+ sha256) to the GitHub release. The same tag also builds **unsigned**
desktop installers (Windows exe, macOS dmg, Linux AppImage/deb) from
[`electron/`](electron/) — see [ADR 014](docs/adr/014-desktop-shell-electron.md)
and [electron/README.md](electron/README.md).

## Desktop

The desktop app is an Electron shell (bundled Chromium — system webviews
break WebRTC/screen-share/E2EE parity on Linux and macOS; details in
[ADR 014](docs/adr/014-desktop-shell-electron.md)). It loads your Huddle
server's web client, so it needs a server to talk to: on first run it asks
for the server address (e.g. `https://meet.example.com` or
`localhost:3000` for a local dev server) and remembers it. Building it
yourself:

```sh
(cd crates/web && trunk build)          # the client the server serves
HUDDLE_WEB_DIR=crates/web/dist cargo run -p huddle-server
cd electron && npm ci && npm run dev     # shell against localhost:3000
```

Dev details, per-OS manual verification checklist, and the version policy
live in [electron/README.md](electron/README.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Unless required by
applicable law or agreed to in writing, software distributed under the License
is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
KIND, either express or implied. See the License for the specific language
governing permissions and limitations under the License.
