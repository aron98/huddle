# ADR 001: Hand-rolled SFU on str0m

- Status: accepted
- Date: 2026-10-13

## Context

Multi-party meetings need a media topology. P2P mesh was rejected outright
(O(n²) uplink; screen share alone would upload 2–5 Mbps × peers from one laptop).
We need an SFU. Candidate approaches:

1. **`str0m`** — pure Rust, sans-io WebRTC library; SFU logic is our own.
2. **mediasoup Rust bindings** — battle-tested C++ SFU via FFI.
3. **`webrtc-rs`** — pure Rust Pion port; SFU logic still our own.
4. **Wrap LiveKit** — proven Go SFU; we'd only orchestrate.

The project's stated goals are: Rust on the server, lightweight self-hosting
(single static binary), and full ownership of the media path (E2EE stays
architecturally possible).

## Decision

Build the SFU ourselves on **str0m**.

## Consequences

**Positive**
- Pure Rust, single static binary — no C++ toolchain, no FFI boundary, matches
  the self-host story.
- Sans-io design: the entire media plane is unit-testable without sockets
  (wire two `Rtc` instances together in-process). This is the single biggest
  risk reducer for the hardest part of the project.
- We own the media path completely — E2EE and future simulcast remain open.
- Small, focused API surface vs. webrtc-rs.

**Negative**
- We hand-write the SFU logic: forwarding tables, PLI fan-out on join,
  late-joiner keyframes, NACK/bandwidth-estimation tuning. This is the project's
  primary schedule risk (see ARCHITECTURE → Risks).
- str0m is pre-1.0: pin a known-good version, batch upgrades deliberately.
- We can't lean on mediasoup's decade of production hardening; synthetic-client
  soak tests in CI compensate.

**Rejected alternatives**
- *mediasoup bindings*: safest SFU, but the C++ build breaks the single-binary
  pure-Rust deployment story. Revisit only if str0m proves a dead end.
- *webrtc-rs*: workable, but async-coupled and heavier; str0m's explicitness
  suits an SFU better.
- *LiveKit*: fastest to ship, but abandons the Rust-server goal and adds a huge
  surface we don't own.

## Amendment (2026-10-14)

- "E2EE and future simulcast remain open": both now have deciding issues in the
  [post-MVP milestone](https://github.com/aron98/huddle/milestone/1) — E2EE
  decision + spike in [#14](https://github.com/aron98/huddle/issues/14),
  simulcast design in [#12](https://github.com/aron98/huddle/issues/12).
