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
  idle RSS), SQLite storage, a Prometheus `/metrics` endpoint, per-IP rate
  limiting, and TURN via coturn for locked-down networks.

**Project status:** the MVP is complete — see the
[post-MVP milestone](https://github.com/aron98/huddle/milestone/1) for what's
next (SSO, simulcast, passcodes, E2EE, recording, and more).

## Stack

| Layer | Choice |
|---|---|
| Media routing (SFU) | Hand-rolled on [`str0m`](https://github.com/lookback-io/str0m) — pure Rust, sans-io WebRTC |
| Server | Rust, `axum` + `tokio`, WebSockets, `sqlx` |
| Storage | SQLite (single file) |
| Web client | Rust → WASM with [`Leptos`](https://github.com/leptos-rs/leptos); browser-native WebRTC via `web-sys` |
| Desktop (later) | Tauri shell reusing the web client |
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
ports) is covered in [Self-hosting](#self-hosting) below.

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

### Ports and firewall

A stock huddle needs exactly three holes in the firewall:

| Port | Protocol | Purpose |
|---|---|---|
| 3000 | TCP | web UI, REST API, and websocket signaling (`HUDDLE_BIND`) |
| 40000 | UDP | SFU media socket (`HUDDLE_MEDIA_PORT`) — one port for all calls |
| 3478 + 49160–49200 | UDP | coturn: TURN listener + relay port range (optional but recommended) |

The two huddle ports must be open to every participant's browser; the coturn
range only matters when you run the TURN companion (below). A browser that
cannot reach the UDP media port directly will fall back to TURN — which is
why coturn matters even for small self-hosted setups behind NAT.

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
refuses loopback peers by default for good reason.

### Docker / Kubernetes

Docker: `docker build -t huddle .` produces the single-binary image; and
`docker compose up` runs a hot-reloading dev stack (web + server + coturn dev
credentials).

Kubernetes: the [Helm chart](deploy/helm/huddle) deploys one pod with the web
client embedded in the binary. It defaults to host networking because a k8s
Service cannot proxy the SFU's UDP media correctly for ICE — the advertised
candidate must be directly reachable. `helm install huddle deploy/helm/huddle
--set server.baseUrl=https://meet.example.com` and check `helm show values`
for the knobs (`server.publicIp`, `server.iceServers`, an existing session
secret, persistence).

Releases: pushing a `v*` tag makes CI build a static musl binary with the web
client baked in and attach `huddle-<version>-x86_64-unknown-linux-musl.tar.gz`
(+ sha256) to the GitHub release.

## License

[MIT](LICENSE)
