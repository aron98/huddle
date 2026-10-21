# AGENTS.md

Compact operational guide for agents working in this repo. Verified against the current tree.

## Layout

Cargo workspace, edition 2024, resolver 3, toolchain pinned in `rust-toolchain.toml` (the single source of truth — CI runs `rustup toolchain install --profile minimal`).

- `crates/protocol` (`huddle-protocol`) — serde wire messages, REST + WS. `PROTOCOL_VERSION = 1`; changes are **additive-only** (new enum variants with `tag="type"` snake_case serde); protocol commits land alone before dependents build on them.
- `crates/sfu` (`huddle-sfu`) — sans-io SFU room on **str0m, pinned `=0.24.1`** (pre-1.0 API churn).
- `crates/server` (`huddle-server`, **binary named `huddle`** — `cargo build -p huddle` fails).
- `crates/web` (`huddle-web`) — Leptos 0.8 CSR, built with trunk to `crates/web/dist` (gitignored). `src/lib.rs` holds landing/login/dashboard; the meeting page is `src/call.rs` (`MeetingFlow` owns the explicit flow state machine in `src/flow.rs`: prejoin → joining → in-call → left/connection-lost, incl. a designed post-leave view), `src/prejoin.rs` the prejoin page. The flow diagram is `docs/user-flow.md` and the code must match it. wasm-only modules are `#[cfg(target_arch = "wasm32")]`; the native stub render keeps `cargo clippy --all-targets` green.

## Commands

```sh
# quality gates — EVERY commit must pass all of these:
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy -p huddle-web --target wasm32-unknown-unknown -- -D warnings   # NO --all-targets here!

# web client build (trunk is a release binary in ~/.local/bin — `cargo install trunk` fails on this VM):
(cd crates/web && trunk build)

# run the server with the real UI:
HUDDLE_WEB_DIR=crates/web/dist cargo run -p huddle-server

# single test / single package:
cargo test -p huddle-server <test_name>
```

**The wasm32 gate must stay lib/bin-only.** `--all-targets` pulls native dev-deps (tokio/mio) into the wasm build and fails with ~48 mio errors.

## Commit / PR discipline

- Atomic commits, conventional style: `feat|fix|test|build|ci|docs(scope): ...`
- **Every commit green through the full gate suite** — the history is bisectable by design.
- Feature work happens on `phase-N/...` or topic branches → PR → all CI jobs green before merge.
- CI is monitored via cache-busted polling (`curl -sH "Cache-Control: no-cache" https://api.github.com/repos/aron98/huddle/actions/runs/...`); `gh run watch` exits misleadingly non-interactively, and plain curl serves stale cache.

## E2E tests (`test/e2e`)

Puppeteer scripts run against the local debug binary. Three suites: `e2e-call.mjs` (:3131), `e2e-auth.mjs` (:3132 — boot env **must** set `HUDDLE_BASE_URL=http://127.0.0.1:3132` or magic links point at an unreachable localhost:3000), `e2e-turn.mjs` (:3133, spawns its own coturn on 3479 via config file).

```sh
cd test/e2e && CHROME_BIN=<chrome-headless-shell> DUMP_SERVER_LOGS=0 node e2e-call.mjs
```

- **Chrome is pinned to `chrome-headless-shell@155.0.8059.39`** (installed via `npx @puppeteer/browsers`). Full `google-chrome` under new headless wedges CDP at the first two-page click (renderer lost-wakeup — not an app bug; single-page runs pass).
- Pages load with `?test=1` to expose `window.__huddlePc` for getStats probes. `?turn=relay` forces relay-only ICE.
- Join is gated on the local stream being live — wait for `[data-testid="join-btn"]:not([disabled])`, then `evaluate(() => el.click())` (coordinate clicks race the preview's first-frame reflow).
- Magic links are **single-use** — a script cannot both fetch and browser-follow the same link.
- `pkill -x huddle` — NEVER `pkill -f huddle` (matches the invoking shell and kills your own session).

## Browser quirks (bitten us all, proven by debugging)

- **`MediaStream.id()` is unstable across reads after `ontrack`** — capture the id ONCE at event time and dedupe/key tiles on that String; duplicate keys in Leptos keyed-`<For>` corrupt the DOM.
- **Chrome's `addTrack` reuses a recvonly transceiver** instead of creating an m-line — use `add_transceiver_…_and_init` (Sendonly + streams) for screen tracks.
- **`setConfiguration` resets `iceTransportPolicy`** (every field of a constructed RTCConfiguration is authoritative) — re-assert the policy when applying the Welcome's ICE servers.
- **Leptos keyed-`<For>` does not re-render same-key children** when the list is refreshed — the child closure must re-read the signal and find its item by key.
- **Signaling pump cancel-safety**: read inside `futures::select!` (`read_server_message`), handle OUTSIDE (`handle_server_message`) — dropping the handle future mid-JS-promise loses SdpAnswers and wedges renegotiation (~50% flake before the fix).
- e2e asserts media by **ssrc snapshots** (`inbound-rtp` framesDecoded) — track identity AND `track.id()` are unusable (provisional wrappers).
- web-sys 0.3.x: builder methods (`.sdp()`, `.ice_transport_policy()`) are deprecated `&mut Self` chains — use `set_*`.
- **web-sys `Clone` for `MediaStream` lowers to JS `MediaStream.clone()`** — a TRACK SNAPSHOT (new live track objects), not a reference clone. Publishing `stream.clone()` into signals/holders means later track mutations miss the copy, and cloned mic tracks silently keep device captures open (pinning Chrome per-device audio-processing state). Clone the JsValue handle instead (`MediaStream::from(JsValue::as_ref().clone())`) for a refcount bump and publish the original into signals.

## SFU / str0m

- **str0m paces RTP emission against real time** — sans-io tests asserting "N frames" need wall-clock drain windows, not frame counts.
- str0m answers browser sendrecv offers recvonly; server-initiated renegotiation offers must wait until the peer `has_session`; renegotiation gated on `pending.is_none()`.
- RTCP (FIR/PLI) is SRTCP-encrypted on the wire — assert keyframes via the SFU-side `keyframes_requested` counter, not by sniffing bytes.
- `Event::MediaChanged` is unreliable for reshare detection (str0m clears the pending flag internally); offer direction (`media_lines[].direction().is_sending()`) is the authoritative signal.
- str0m includes all ICE candidates inline in SDP — no server→client trickle on the SFU side.

## Server

- Config via figment: TOML (`huddle.toml`) + `HUDDLE_*` env. **Env arrays-of-structs need figment's value syntax in ONE var** — `HUDDLE_ICE_SERVERS='[{urls=["turn:h:3478"],username="x",credential="y"}]'` (TOML-ish `key=value`, NOT JSON colons — colons fail the parse and the server exits). `__0__`-style key nesting silently never reaches `Vec` fields.
- Both serve sites (main.rs + session tests) use `into_make_service_with_connect_info::<SocketAddr>()` — handlers use `ConnectInfo` for rate limiting.
- axum: `Json` extractor failures are **422** (not 400); routes must be added **before** `.with_state()`; `WebSocketUpgrade` caps are chainable builder methods (`.max_frame_size/.max_message_size`), no `configure()`.
- sqlx: `migrate!` needs `"./"`-prefixed paths; SQLite schema stores UUIDs as TEXT (`.to_string()`/`Uuid::parse_str`) and times as RFC3339 TEXT; tests needing owned meetings must insert real organizer rows (FK enforced).
- Sessions/auth: magic-link tokens stored sha256-hashed, single-use, atomic claim; session cookie `v1.<uuid>.<email>.<unix-expiry>.<hmac>` — parse **right-to-left** (emails contain dots); the HMAC covers the full payload including expiry.
- The CSP on served UI assets allows trunk's inline bootstrap by a **runtime-computed sha256 hash** (`ui_csp` in http.rs) — a static policy breaks the app, and the render stub never executes JS so only the full e2e catches it.
- WS message cap: 256 KiB. Per-IP rate limiting: magic-link + new WS connections (30/min default, 0 disables).

## Deployment / infra

- Helm chart `deploy/helm/huddle` — linted in CI; hostNetwork by default (a k8s Service cannot proxy SFU UDP for ICE).
- Release workflow on `v*` tags: alpine musl static binary with the web client **baked in** via rust-embed (`crates/web/dist` → `crates/server/assets`).
- Trunk **release** builds need `data-wasm-opt-params="--enable-bulk-memory-opt"` (already on the rust link in `index.html`) — trunk's pinned binaryen rejects `memory.copy` without it; debug builds skip wasm-opt.

## Desktop shell (`electron/`)

Electron shell (ADR 014) that **loads a Huddle server's HTTP origin** — the web client is same-origin by design, so the shell never serves `crates/web/dist` itself. NOT a Cargo workspace member; it has its own npm package, never touches the Rust gates.

- Exact pins: `electron@45.0.0` (Chromium 156.0.8078.48) and `electron-builder@26.15.3`; bump deliberately, keep Chromium ≥ 141 (E2EE floor, ADR 009), and record the new Chromium version in `electron/README.md`.
- Main-process modules are ESM `.mjs`; preloads are sandbox-compatible plain `.js` (package.json has no `"type"`). Pure logic (settings, deep-link parsing) is fs-injected and covered by `node --test`.
- Gates (CI `desktop` job, no `npm install` needed): `cd electron && npm run check && npm test`.
- No E2E: releases rely on the manual per-OS checklist in `electron/README.md` — keep it in sync with shell behaviour.
- Known web-side gap: the client hard-codes `ws://` (see `crates/web/src/call.rs`), so the shell only works against plain-HTTP/localhost servers until that is fixed web-side.

## VM quirks (this environment)

- `/tmp` is tmpfs with a tight quota — clean failed cargo installs before big builds.
- crates.io REST API is blocked; use the sparse index `https://index.crates.io/{a}/{b}/{name}`.
- `cargo build && ./target/debug/huddle &` backgrounds the **whole chain** — build in the foreground, then background the server separately.
