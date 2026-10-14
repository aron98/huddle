# huddle

[![ci](https://github.com/aron98/huddle/actions/workflows/ci.yml/badge.svg)](https://github.com/aron98/huddle/actions/workflows/ci.yml)

**Lightweight, self-hostable virtual meetings — in Rust.**

huddle is an open-source video meeting application you can run as a single binary
on your own server: one process, one SQLite file, one TURN companion. No JVM, no
Node farm, no 40-step install guide.

## Status

🚧 **Phases 0–3 complete** — the foundations, the four independent tracks, the
two-person call, and n-party support from the
[implementation plan](docs/implementation-plan.md) are done:

- `protocol` — v1 signaling and REST wire messages, including speaking
  indicators.
- `server` — meeting CRUD API with join windows, SQLite via sqlx, embedded web
  UI, `/ws` signaling sessions, and a media engine thread driving one SFU room
  per meeting over UDP.
- `sfu` — sans-io SFU room on [str0m](https://github.com/lookback-io/str0m): peers,
  SDP answering, media forwarding, keyframe fan-out to new subscribers, track
  recycling when participants leave, mute-gated forwarding; tested with
  synthetic in-process clients and a real-UDP loopback.
- `web` — Leptos prejoin page (camera preview, device pickers, name), an
  `RtcClient` signaling driver tested against a mock server, and an in-call
  view with an adaptive tile grid, roster, speaking indicators (Web Audio RMS
  detection), and mic/camera/leave controls.
- Infra — CI (fmt, clippy native + wasm32, tests, headless-Chrome render check,
  and a four-browser-page call e2e with join/leave churn that asserts media
  flows *through the server*), multi-stage Dockerfile, dev compose stack with
  coturn.

**Next:** phase 4 — screen share and chat fan-out.

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

Configuration is `HUDDLE_*` environment variables (optionally a `huddle.toml`):
`HUDDLE_BIND` (default `0.0.0.0:3000`), `HUDDLE_DATA_DIR` (default `./data`),
`HUDDLE_PUBLIC_IP`, `HUDDLE_MEDIA_PORT` (default `40000`), `HUDDLE_WEB_DIR`
(serve a web bundle from disk instead of the embedded one).

Docker: `docker build -t huddle .` produces the single-binary image; and
`docker compose up` runs a hot-reloading dev stack (web + server + coturn dev
credentials).

## License

[MIT](LICENSE)
