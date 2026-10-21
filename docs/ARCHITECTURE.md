# huddle — Architecture

Design date: 2026-10-13 · Status: implemented — the MVP is complete; post-MVP
work is tracked in the [post-MVP milestone](https://github.com/aron98/huddle/milestone/1).

## Goals & non-goals

**Goals**
- Lightweight: single static binary, ~tens of MB; idle RAM well under 100 MB for a handful of rooms.
- Self-hostable with minimal ops: binary + SQLite file + coturn + built-in TLS (rustls + ACME — [ADR 008](adr/008-built-in-tls-termination.md)). Docker-compose for the lazy path.
- Performant multi-party meetings: SFU topology, no transcoding.
- Modern web-first client in Rust/WASM.

**Non-goals (MVP)**
- Recording, live captions, whiteboards.
- E2EE (architecturally kept possible — see Security).
- Calendar recurrence parsing (ICS *export* only).
- MCU-style mixing, transocding, server-side codecs.

## Topology: SFU

P2P mesh was rejected: each client uploads its stream to every peer (O(n²) uplink),
which caps meetings at ~4–6 participants and makes screen share — 2–5 Mbps that would
be multiplied by peer count — untenable.

huddle uses a **Selective Forwarding Unit (SFU)**: each client sends its media once to
the server; the server forwards each stream to the other participants without decoding
or mixing. CPU cost scales linearly with *subscribers*, not quadratically with peers.

## Workspace layout

```
huddle/
├── crates/
│   ├── protocol/   # serde message types shared by server & WASM client (no server-only deps)
│   ├── sfu/        # str0m media plane: Router → Room → Peer
│   ├── server/     # bin: axum HTTP+WS, sqlx/SQLite, magic-link auth, embedded static UI
│   └── web/        # lib→cdylib: Leptos app compiled to WASM
├── electron/       # desktop shell (Electron) loading the server's web client — not a Cargo member
└── deploy/         # docker-compose, coturn config, Helm chart
```

The `protocol` crate is the contract: every message crossing the WebSocket is defined
there and compiled into both server and client. Interface drift is a compile error.

## Process model

One process, two planes:

- **Control plane** — axum/tokio: HTTP (scheduling, auth, static UI) and the
  signaling WebSocket per participant.
- **Data plane** — str0m is *sans-io*: the SFU owns a UDP socket and drives each
  `Rtc` instance from a dedicated task, feeding incoming packets and polling
  `Rtc::handle_timeout()`. No hidden threads; all I/O is explicit and testable.

The signaling task and the media task for a participant communicate via channels.
Media never touches the control plane except for statistics.

## Signaling protocol

Carried over the same WebSocket as chat. Wire format: JSON for MVP (debuggable in
the browser devtools), swappable for MessagePack later without touching message
semantics. All types live in `protocol`.

Message families:

| Family | Direction | Purpose |
|---|---|---|
| `session` | ⇄ | join (meeting code, display name, optional token), joined, roster, peer-leave |
| `media` | client→server | SDP offer, ICE candidates |
| `media` | server→client | SDP answer, ICE candidates, subscribe/track notifications, mute requests |
| `chat` | ⇄ | chat messages, system events ("Alice joined", "Bob is sharing") |
| `error` | server→client | join refused, meeting window closed, capacity |

**Peer connection pattern (MVP):** one `RTCPeerConnection` per participant.
The browser publishes camera, mic, and (optionally) screen on it; the SFU answers
and renegotiates as other participants join/leave to add forwarded tracks.
A publish/subscribe split across two PCs is a known escape hatch if renegotiation
churn becomes a problem.

**ICE:** the server's host candidates are known up front (static public IP), so its
SDP answer carries complete candidates; the client trickles via `media` messages.
Server media runs over a single UDP port per process.

**Keyframe discipline:** the SFU issues PLIs on subscribe and on new-subscriber
join to avoid "frozen tile" bugs; NACK retransmission and TWCC bandwidth estimation
are handled by str0m. The SFU logic we write ourselves:

- track forwarding tables (who subscribes to whose SSRCs)
- PLI fan-out on join
- mute/unmute signaling (server stops forwarding muted tracks)
- late-joiner catch-up

**Codecs:** VP8 + Opus for MVP (universal browser support). Simulcast is post-MVP,
but `media` messages already carry layer/rid fields so it can be added without
breaking the protocol.

## Data model (SQLite via sqlx)

```sql
organizers(id, email, created_at)
login_tokens(hash, expires_at)                        -- magic links
meetings(id, code, title, starts_at, ends_at, organizer_id, created_at, cancelled)
chat_messages(id, meeting_id, sender, body, sent_at)
```

- Meeting `code` is the join code in the URL: `https://host/m/{code}`.
- Joinable window: `starts_at − 5min` → `ends_at + 15min` grace (configurable).
- No recurrence in MVP; ICS export ("Add to calendar") is generated per meeting.
- Guests are anonymous: display name only, no account. The meeting link is the
  capability (plus optional per-meeting passcode, post-MVP).

## Identity

Behind a trait from day one:

```rust
trait IdentityProvider {
    async fn create_login(email: Email) -> Result<LoginToken>;
    async fn verify(token) -> Result<OrganizerSession>;
    // future: verify_oidc(code) -> OrganizerSession
}
```

MVP implementation: magic link (email with one-time token → short-lived session
cookie). The post-MVP OIDC/SAML integration (company SSO) is a new impl of this
trait, not a rewrite. Anonymous guests never touch `IdentityProvider`.

## Client architecture (Leptos → WASM)

- The browser's `RTCPeerConnection` is driven via `web-sys` — browsers do not
  expose raw RTP, so the client media stack *is* the browser's (codecs, AEC,
  camera/screen capture come for free).
- A thin, dumb `RtcClient` module owns all WebRTC interop; Leptos components
  consume signals it produces. This keeps the interop layer targetable by
  headless-Chrome E2E tests.
- Prejoin page: device pickers, preview, name entry. In-call: adaptive video tile
  grid, mute/camera toggles, screen share button, chat sidebar.
- **Bundle budget:** < 300 KB gzipped WASM+JS. No heavyweight component libraries.
- Desktop (post-MVP): an Electron shell loads the same web UI from the
  server's origin ([ADR 014](adr/014-desktop-shell-electron.md)). A native
  Rust client (egui + webrtc-rs) was rejected for MVP — it would re-implement
  the entire media stack; a system-webview shell (Tauri) was rejected because
  the OS engines break media/E2EE parity on Linux and macOS.

## Deployment targets

| Target | Shape |
|---|---|
| Own VPS (primary) | Single binary + SQLite + coturn, HTTPS terminated by the binary itself (rustls, Let's Encrypt via ACME — [ADR 008](adr/008-built-in-tls-termination.md)). A reverse proxy is an optional extra, no longer required. `docker-compose.yml` provided. |
| AWS | EC2 or Fargate task + ALB (WebSocket-aware), coturn sidecar on the same host (must see its public IP in ICE candidates). |
| Kubernetes | Helm chart (post-MVP hardening phase). |
| Cloudflare | Not a primary target: full self-host keeps the media path open for E2EE. A Workers/D1 signaling port remains possible later. |

Step-by-step production guides per platform: [Ubuntu VPS](deploy/ubuntu-vps.md),
[Docker Compose](deploy/docker-compose.md), [Kubernetes](deploy/kubernetes.md), plus a
cross-platform [hardening checklist](deploy/hardening-checklist.md) — see
[docs/deploy/](deploy/).

**Firewall ports (self-host story — document early):**

| Port | Protocol | Purpose |
|---|---|---|
| 443 | TCP | HTTPS + WebSocket signaling (terminated by the binary in TLS mode — [ADR 008](adr/008-built-in-tls-termination.md)) |
| 3478 | UDP/TCP | coturn STUN/TURN (5349 TCP additionally, if TURN over TLS is enabled) |
| 49160–49200 | UDP | coturn TURN relay allocations |
| 40000 (config) | UDP | SFU media (single ICE port) |

## Performance levers (in order)

1. VP8 + Opus (compat) → AV1 later.
2. Simulcast (server picks layer per subscriber bandwidth) — the biggest win, post-MVP.
3. TWCC-based bandwidth estimation (str0m support).
4. Keyframe discipline (PLI on subscribe/join).
5. Bundle budget + rust-embed static serving (no extra web tier).

## Testing strategy

- **Unit:** str0m's sans-io design lets SFU logic be tested without sockets — wire
  two `Rtc` instances together in-process and assert forwarding behavior.
- **Integration:** native synthetic clients (str0m-based load harness) for soak tests.
- **E2E:** headless Chrome multi-tab in CI — 3 tabs join a room, assert via
  `getStats()` that frames flow both directions, mute a tab, share a screen.

## Security

- Media is always DTLS-SRTP (handled by the stack).
- TLS: HTTPS/WSS terminated in-process by rustls ([ADR 008](adr/008-built-in-tls-termination.md)); in TLS mode responses carry HSTS and session cookies are marked `Secure`.
- Organizers: magic-link tokens hashed at rest, short TTL; session cookie.
- Meeting links are capabilities; per-meeting passcodes post-MVP.
- Rate-limit join attempts + WS message size caps.
- E2EE (insertable streams, à la Jitsi) is *architecturally possible* only in the
  self-hosted SFU path — a managed-media port (e.g. Cloudflare Calls) would
  preclude it. Decide before ever promising it — tracked by
  [issue #14](https://github.com/aron98/huddle/issues/14).
- For a deployment-oriented view of these guarantees (HSTS/cookie behavior,
  rate limits, log hygiene, backups, upgrade path), see the
  [hardening checklist](deploy/hardening-checklist.md).

## Risks

| Risk | Mitigation |
|---|---|
| SFU correctness (PLI storms, NACK buffers, BWE padding) is the schedule risk | Milestone 2 isolates it; sans-io unit tests; synthetic-client soak tests |
| str0m is pre-1.0 — API churn | Pin a known-good version; batch upgrades; upstream is responsive |
| Leptos ↔ async/WS boundary ergonomics | Thin `RtcClient` wrapper; budgeted exploration time in milestone 2 |
| Scope creep (recording, captions, …) | The MVP shipped against a fixed definition of done; new scope enters through the post-MVP milestone, not in-flight PRs |
