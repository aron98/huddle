# huddle

[![ci](https://github.com/aron98/huddle/actions/workflows/ci.yml/badge.svg)](https://github.com/aron98/huddle/actions/workflows/ci.yml)

**Lightweight, self-hostable virtual meetings — in Rust.**

huddle is an open-source video meeting application you can run as a single binary
on your own server: one process, one SQLite file, one TURN companion. No JVM, no
Node farm, no 40-step install guide.

## Status

🚧 **Phases 0–5 complete** — the foundations, the four independent tracks, the
two-person call, n-party support, screen share + chat, and scheduling with
organizer auth from the [implementation plan](docs/implementation-plan.md) are
done:

- `protocol` — v1 signaling and REST wire messages, including speaking
  indicators and screen-share state.
- `server` — meeting CRUD API with join windows, SQLite via sqlx, embedded web
  UI, `/ws` signaling sessions, a media engine thread driving one SFU room
  per meeting over UDP, chat persistence with history replay on join,
  magic-link login with hashed single-use tokens (lettre delivery with a log
  fallback), signed session cookies, organizer-owned meetings, and ICS export.
- `sfu` — sans-io SFU room on [str0m](https://github.com/lookback-io/str0m): peers,
  SDP answering, media forwarding, keyframe fan-out to new subscribers, track
  recycling when participants leave, mute-gated forwarding, and a screen-share
  lifecycle (labeled `-screen` streams, reshare on the same transceiver,
  screen tracks immune to camera mute); tested with synthetic in-process
  clients and a real-UDP loopback.
- `web` — Leptos prejoin page (camera preview, device pickers, name), an
  `RtcClient` signaling driver tested against a mock server, and an in-call
  view with an adaptive tile grid, roster, speaking indicators (Web Audio RMS
  detection), screen share with dedicated tiles, an in-call chat sidebar, and
  mic/camera/share/leave controls — plus a login page and a meeting dashboard.
- Infra — CI (fmt, clippy native + wasm32, tests, headless-Chrome render check,
  a four-browser-page call e2e with join/leave churn, screen share, and
  chat-across-reload assertions that verify media flows *through the server*,
  and a magic-link + dashboard e2e), multi-stage Dockerfile, dev compose stack
  with coturn.

**Next:** phase 6 — hardening (coturn guide, `/stats`, security, Helm).

- [README](README.md) — you are here
- [ARCHITECTURE](docs/ARCHITECTURE.md) — system design
- [Implementation plan](docs/implementation-plan.md) — build order and parallelization
- [ADRs](docs/adr/) — the decisions we've made and why

## MVP scope

| Feature | In |
|---|---|
| Scheduled meetings (magic-link login, ICS export, join windows) | ✅ |
| Camera + microphone for all participants | ✅ |
| Screen sharing | ✅ |
| Chat (relayed + persisted per meeting) | ✅ |
| Recording, E2EE, simulcast, calendar recurrence, OIDC/SAML | ❌ post-MVP |

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

Docker: `docker build -t huddle .` produces the single-binary image; and
`docker compose up` runs a hot-reloading dev stack (web + server + coturn dev
credentials).

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

## License

[MIT](LICENSE)
