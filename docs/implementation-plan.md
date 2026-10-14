# huddle — Implementation Plan

Companion to [ARCHITECTURE](ARCHITECTURE.md). Answers two questions: **what order**,
and **where can work run in parallel**.

## Guiding principles

1. **Retire risk earliest.** The single riskiest seam in the project is
   WASM↔native WebRTC interop through a hand-rolled SFU. Everything about the plan
   front-loads reaching a *two-person call working end-to-end* (Phase 2). All other
   features build on that spine.
2. **Parallelize via contracts, not hope.** Tracks diverge only where a stable
   interface exists between them. That interface is the `protocol` crate (wire
   messages) plus the `RtcClient` boundary in the web client. Rule: **protocol
   changes always land alone, in their own small PR, before any track builds on
   them.** This is the only cross-track coupling in the codebase.
3. **The SFU track never blocks on the web client, and vice versa.** The SFU is
   testable with native synthetic str0m clients; the web client is testable with
   `getUserMedia` loopback and a mock signaling server. They meet only at the
   scheduled convergence points.

## Tracks

| Track | Owns | Testable without other tracks? |
|---|---|---|
| **A — Media** (`sfu` + `server` WS) | str0m peers, forwarding, PLI, renegotiation, mute | Yes — native synthetic clients (str0m↔str0m in-process) |
| **B — Web** (`web`) | Leptos app, `RtcClient` interop, prejoin, tiles, chat UI | Yes — local preview + mock signaling server |
| **C — Data** (`server` HTTP) | SQLite schema, meeting CRUD, join windows, magic-link auth, ICS | Yes — pure HTTP/JSON, curl-able |
| **D — Infra** | CI, docker-compose, coturn config, `/stats`, Helm | Yes |

## Dependency graph

```
Phase 0  workspace + protocol skeleton + CI ──────────────┐
   │                                                    │
   ├─→ A1: SFU peer answers a synthetic client ──┐       │
   ├─→ B1: Leptos shell + camera preview +       │       │
   │        WS client against mock server        ├─→ S1: TWO-PERSON CALL  ← risk retired
   ├─→ C1: meeting CRUD API (curl-able)          │       │
   └─→ D1: CI + headless-chrome harness stub ────┘       │
                                                         │
Phase 3  A2: N-party forwarding/renegotiation ──┬─→ S2: N-PARTY DEMO
         B2: tile grid, toggles, roster ────────┘        │
                                                         │
Phase 4  A3: screen-share forwarding ──┐                 │
         B3: getDisplayMedia + UI ──────┼─→ S3            │
         C2+B4: chat (persist + UI) ───┘   parallel feature fan-out
                                                         │
Phase 5  C3: magic-link auth + scheduling UI + ICS       │
         (independent of media entirely)                  │
                                                         │
Phase 6  D2: coturn guide, compose, stats, security, Helm, docs
```

**Critical path:** A1 → S1 → A2 → S2 → A3 → S3. Track B shadows it; tracks C and D
float freely across all phases.

## Phases

### Phase 0 — Foundations *(~1 week, mostly serial)* ✅

Everything later depends on this; it's small.

- Cargo workspace: `protocol`, `sfu`, `server`, `web` crates. `cargo check`,
  `clippy`, and `trunk`/`wasm-pack` builds all green.
- `protocol` v0: all message families stubbed (types with placeholders, no logic).
  This is what unblocks parallel fan-out — worth doing *before* the crates are useful.
- CI skeleton: fmt, clippy, native tests, WASM build, and a **headless-Chrome
  harness stub** wired up before there's any media to test.
- Dev config: single TOML/env, `rust-embed` override dir for web assets.

Parallel within the phase: infra (D) and protocol work can proceed concurrently;
crate scaffolds are quick and serial.

### Phase 1 — Independent spin-up *(1–2 weeks, fully parallel A/B/C/D)* ✅

- **A1 (SFU proof, native-only):** `Rtc` answered by SFU code; synthetic client
  publishes audio+video, SFU echoes it back. In-process, no sockets needed for the
  unit test; one real UDP loopback integration test. Done when a str0m-native
  client gets its own media forwarded back.
- **B1 (Web shell):** Leptos app with prejoin page — camera preview, device
  pickers, name entry — plus `RtcClient` (WS connect, SDP offer/answer plumbing)
  tested against a tiny mock signaling server. Done when the prejoin page shows
  your camera and round-trips an SDP offer to the mock.
- **C1 (Data API):** sqlx schema + migrations, meeting CRUD endpoints, join-window
  resolution (`starts_at − 5min .. ends_at + grace`), meeting-code generation.
  Curl-only, no UI.
- **D1:** CI from Phase 0 hardened; docker-compose with server+web hot-reload.

### Phase 2 — **S1: Two-person call** *(~1–2 weeks; convergence)* ✅

Done. The Leptos prejoin page talks to the real SFU through the real WS;
this milestone retired the interop, ICE, SDP, WASM-async, and renegotiation
risks.

- A: WS signaling glue in `server`; session join; single-PC publish; forward one
  peer's tracks to the other. *(Landed: media engine thread + `/ws` sessions,
  proven by a synthetic two-client integration test against the full stack.)*
- B: minimal in-call page — two tiles, mute/camera toggle, roster. *(Landed.)*

**Acceptance:** two Chrome tabs join a room; each sees and hears the other *through
the server* (verify via `getStats()` in the CI harness — not P2P); mute stops the
track at the SFU. This E2E test becomes the CI gate forever after.
*(Landed: `e2e-call` CI job — two headless-Chrome pages, inbound-rtp bytes
asserted on both, mute propagation asserted across pages.)*

### Phase 3 — **S2: N-party** ✅ *(~2 weeks; A and B parallel, converge at the end)*

- **A2:** join/leave renegotiation (add/remove forwarded tracks), forwarding
  tables, PLI fan-out on subscribe + on new-subscriber join (late joiner gets a
  keyframe, not a frozen tile), mute stops forwarding, speaking-indicator events.
  *(Landed: stop-media renegotiation on departure (`341767c`), keyframe requests
  from sources on every new subscription (`f5be869`), mute-gated forwarding at
  the SFU (`ea855a6`), speaking relayed through the engine (`2ba58d7`).)*
- **B2 (parallel):** adaptive tile grid, roster, speaking indicator UI.
  *(Landed: Web Audio speaking detection + tile ring (`9abd9e1`, `f14f7f7`),
  tile cleanup on `ParticipantLeft` (`351f800`).)*
- Converge: E2E test grows to 4 tabs with join/leave churn (join late → no frozen
  tiles; leave → tiles clean up).
  *(Landed: `e2e-call` CI job now runs four pages — late joiner receives fresh
  media, departure removes tiles and roster rows everywhere (`7bd28bd`).)*

### Phase 4 — Feature fan-out *(~2 weeks; two features in parallel)*

- **Screen share:** A3 (forward high-bitrate track; treat screen content
  differently — no simulcast budget games, generous max bitrate, keyframe on
  subscribe) runs in parallel with B3 (`getDisplayMedia`, listen for the browser's
  own "stop sharing" signal via track `onended`, screen tile treatment, "X is
  sharing" system event).
- **Chat:** C2 (persistence) + B4 (sidebar UI) run in parallel with the above —
  chat is pure WS + SQLite and doesn't touch media at all.

**Acceptance:** share a screen in a 4-tab meeting, chat persists across reload.

### Phase 5 — Scheduling & identity *(~1–2 weeks; C + B, media tracks idle)*

- C3: magic-link login flow, session cookies, meeting CRUD pages, ICS export,
  join-window enforcement at the WS layer.
- B5: auth pages + meeting management UI (SSR-friendly pages can start here).

### Phase 6 — Ship hardening *(~1–2 weeks; mostly serial, D-led)*

- coturn config + the firewall-ports README section; TURN-reliayed call test.
- `/stats` JSON endpoint (rooms, peers, bitrate, loss) — the self-hoster's
  observability.
- Security pass: rate limits, WS message caps, token TTLs, CSP.
- docker-compose (final shape), Helm chart, release pipeline (single static
  binary + release tarball).

## Parallelization summary

| Phase | Concurrent tracks | Constraint |
|---|---|---|
| 0 | infra ∥ protocol | scaffold itself is serial |
| 1 | A ∥ B ∥ C ∥ D | only protocol stubs shared |
| 2 | A ∥ B | **converge deliberately** — this is the sync point |
| 3 | A ∥ B | converge on renegotiation semantics |
| 4 | (A3+B3) ∥ (C2+B4) | two independent features |
| 5 | C ∥ B | media untouched |
| 6 | D-led, serial | release checklist |

Where parallel work *cannot* help: Phase 0, the S1 convergence itself, and
renegotiation semantics in Phase 3 (SFU and client must move in lockstep — land the
protocol change first, then both sides in quick succession).

Solo notes: phases marked "parallel" are parallelizable but not mandatory-parallel;
solo, do A1 → B1 → C1 in that order (risk first). For pair/multi-agent work: the
A/B split is the natural ownership boundary, C and D are pick-up work.

## MVP definition of done

- [ ] Create a meeting as an organizer (magic link), get a link + ICS file
- [ ] Guest joins from link with just a name, inside the join window
- [ ] 6 participants: camera + mic each, stable tiles, no frozen-tile joins
- [ ] Screen share visible to all; browser "stop sharing" handled
- [ ] Chat works and survives reload within a meeting
- [ ] Two tabs on one machine AND two machines across NATs (via TURN) both work
- [ ] Single binary release + docker-compose documented and tested on a clean VPS
- [ ] E2E CI: 4-tab headless Chrome join/mute/share/chat green on every PR
- [ ] Idle server RSS < 100 MB; WASM bundle < 300 KB gzipped

## Post-MVP backlog (ordered)

1. OIDC/SAML `IdentityProvider` (per ADR 003 — designed for)
2. Simulcast (protocol already carries layer fields)
3. Per-meeting passcodes
4. E2EE via insertable streams (self-host path only — decide before promising)
5. Recording (SFU → ffmpeg sidecar)
6. Cloudflare Workers/D1 signaling port
7. Tauri desktop shell
8. Recurring meetings (RRULE)
