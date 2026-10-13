# huddle

**Lightweight, self-hostable virtual meetings — in Rust.**

huddle is an open-source video meeting application you can run as a single binary
on your own server: one process, one SQLite file, one TURN companion. No JVM, no
Node farm, no 40-step install guide.

## Status

🚧 **Design phase** — the architecture and implementation plan are agreed; no working code yet.

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

## License

[MIT](LICENSE)
