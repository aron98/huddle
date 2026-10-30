# ADR 015: Adaptive simulcast layer selection on str0m's TWCC estimate

- Status: proposed
- Date: 2026-10-30

## Context

Story [#111](https://github.com/aron98/huddle/issues/111): the simulcast
layer policy keys only off the client's focus signal. `UiAction::Focus` in
`crates/web/src/call.rs` sends an explicit `LayerPreference { layer: Low }`
for every unfocused participant and `High` for the focused one, and the SFU
opens every subscription on `VideoLayer::Low` (`Peer::open_track`,
`crates/sfu/src/peer.rs`). In a two-person call where nobody focuses a
tile, both sides watch a 160p thumbnail.

The target is the usual SFU model (Jitsi, LiveKit, mediasoup). Every
receiver gets the high layer by default. The SFU lowers a receiver's
layers only when its measured downlink cannot carry them, and raises them
again once the link recovers, with hysteresis. Focus decides who gets the
scarce bandwidth. It no longer triggers the high layer on its own.

This ADR is the design gate for #112. It records what str0m **0.24.1**
(pinned `=0.24.1`) actually exposes, using the vendored source in
`~/.cargo/registry/src/*/str0m-0.24.1` as the ground truth. It then fixes
the decision loop that #113–#117 implement.

### What exists today

| Piece | Where | Behaviour |
|---|---|---|
| Layers | `huddle_protocol::VideoLayer { Low, High }` | Two layers on the wire. |
| Sender encodings | `crates/web/src/rtc/browser.rs` `publish_camera` | Two layers fixed at transceiver creation: rid `high` scale 1.0 at **500 kbps** max, rid `low` scale 4.0 at **100 kbps** max. They cannot change later: `setParameters` is forbidden on E2EE connections (ADR 011, D1). |
| Rid order | `TrackIn::rids` | Highest quality first. `chosen_rid` maps `High → first`, `Low → last`. |
| Per-receiver choice | `TrackOut::layer` | One `VideoLayer` per outgoing camera track. Initialised to `Low`. |
| Switch | `Room::set_layer_preference` → `Peer::set_layer_preference` | Sets `TrackOut::layer` and sends a throttled FIR on the new rid. `Peer::wants_layer` filters frames by rid **from the next frame on**. Nothing waits for a keyframe, so the receiver decodes inter-frames of the new layer against the wrong reference until the FIR keyframe arrives. |
| Client message | `ClientMessage::LayerPreference { origin, layer }` → `session.rs` → `engine.layer_preference` | One message per (receiver, origin). |
| Metric | `huddle_peer_simulcast_layer{room,peer,id,origin}` (`metrics.rs`) | 0 = low, 1 = high, read from `Peer::layer_preferences`. |
| e2e | `test/e2e/e2e-call.mjs` "simulcast layers switch on tile focus" | Asserts **low before focus**, high after focus, low after unfocus. Uses `sfuSimulcastLayer()` metric polling and frame-width checks on the same ssrc, gated on sender liveness (#78). |
| BWE | `Peer::new` | **Not enabled.** No `enable_bwe`, null pacer. `PeerStats` feeds `LinkStats`. `Event::MediaIngressStats`, `MediaEgressStats` and `EgressBitrateEstimate` all fall into `_ => Ignored`. |

### str0m 0.24.1 BWE surface (verified in source)

**Enabling.** `RtcConfig::enable_bwe(initial_estimate: Option<Bitrate>)`
(`src/config.rs:468`). `Some(b)` creates the estimator. `None`, the
default, disables it. `RtcConfig::bwe_initial_bitrate()` reads the value
back. Enabling BWE **also replaces the null pacer with a leaky-bucket
pacer** (`Session::new`, `src/session.rs:168`). The pacer starts at
`2 × initial` and afterwards paces at `max(1.1 × estimate, padding)`
(`pacer/control.rs`: `PACING_FACTOR = 1.1`, `PADDING_TARGET = 50 kbps`
while media is active). This changes the forwarding path for every peer;
see Consequences.

**Estimator.** `bwe_::Bwe` is a port of libwebrtc GoogCC (`src/bwe/mod.rs`).
It combines a trendline delay controller with AIMD, a loss controller, the
probe controller state machine, ALR detection and a link-capacity
estimator. It is fed exclusively by TWCC feedback:
`Session::handle_rtcp` → `twcc_tx_register.apply_report` → `bwe.update`.
**It estimates one number per `Rtc`, i.e. per transport.** It has no
per-m-line, per-track or per-layer estimate, and it never recommends a
layer.

**Output.** `Event::EgressBitrateEstimate(BweKind)` (`src/lib.rs:1014`),
with `str0m::bwe::BweKind` (`src/bwe/api.rs`, `#[non_exhaustive]`):

```rust
pub enum BweKind {
    Twcc { estimate: Bitrate, can_probe: bool },
    Remb { estimate: Bitrate, mid: Mid },
}
```

- `Twcc` is emitted from `Session::poll_event` **before** the
  `ready_for_srtp` gate. The value comes from an `EstimateSmoother`
  (`src/bwe/smoother.rs`): the mean over a **3 s window**, emitted only
  when it moves more than **5 %** away from the last emitted value. **The
  event is therefore edge-triggered, not periodic.** In the spike, a
  steady link produced no event for over 10 s.
- `can_probe` is `bwe.is_some() && srtp_tx.is_some() && probe_media().is_some()`.
  `probe_media` needs a sending, non-stopped m-line whose remote extmap
  carries `TransportSequenceNumber` and whose PT has `fb_transport_cc`.
  When probing becomes unavailable, the last estimate is emitted once with
  `can_probe: false` and further updates are suppressed.
- `Remb` is emitted per `mid` when the remote sends REMB. huddle ignores
  it: Chrome uses transport-cc whenever it is negotiated.

**Controls.** `Rtc::bwe() -> str0m::bwe::Bwe<'_>` (`src/lib.rs:2141`)
provides:

- `set_desired_bitrate(Bitrate)`: the ceiling the probe controller tries to
  reach with padding probes. **Without it, the estimator only learns from
  the media actually sent.** A receiver that was downshifted to a 100 kbps
  layer can then never discover that it could carry 500 kbps again, so
  re-upgrade depends on setting this. `ProbeControl::set_desired_bitrate`
  ignores an initial zero.
- `reset(init_bitrate: Bitrate)`: discards estimator state. huddle does not
  need it.

**Side channels.** `Event::PeerStats` (1 s, already enabled through
`set_stats_interval`) carries `bwe_tx: Option<Bitrate>`. This is the
*unsmoothed* `bwe.last_estimate()`, polled periodically. The same event
carries `egress_loss_fraction` (TWCC, last 1 s) and `rtt`.
`Event::MediaIngressStats { mid, rid, bytes, packets, … }` (`src/stats.rs:185`)
gives per-(mid, rid) ingress byte counters on the **origin's** `Rtc`, from
which huddle can compute a measured per-layer bitrate.
`Event::MediaEgressStats` gives per-(mid, rid) egress counters plus RR loss.

**`Bitrate`** is `str0m_proto::Bitrate`, re-exported as `str0m::bwe::Bitrate`.
It provides `bps/kbps/mbps` constructors, `as_u64`, `as_f64`, `min`, `max`,
`clamp`, `Mul<f64>`, `Add`, `Sub` and `Display`.

**TWCC negotiation.** str0m's default extmap puts `TransportSequenceNumber`
at id 3 (`src/rtp/ext.rs:375`). `PayloadParams` defaults
`fb_transport_cc = true` for both audio and video (`codec_config.rs:129`).
Receive-side TWCC feedback is switched on session-wide in `change/sdp.rs:1240`
when any m-line has `transport-cc` and the session has the extension. The
feedback interval is 50 ms. Chrome, Edge and Electron (Chromium ≥ 141,
ADR 009/014) offer
`a=extmap:… http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01`
and `a=rtcp-fb:* transport-cc` by default. Firefox and Safari do as well.
**Clients need no change.** The SFU's renegotiation offers come from
str0m's own codec config, so the outgoing m-lines carry the same feedback
attributes.

**Spike (throwaway, not committed).** Two in-process `Rtc`s ran on a
virtual clock. The sender had `enable_bwe(300 kbps)` and sent about
600 kbps of VP8 to a default-config str0m receiver.

- The receiver's recvonly offer contained `transport-cc` and the TWCC
  extmap. `Twcc { can_probe: true }` events fired from t ≈ 0.
- Without `set_desired_bitrate`, the estimate climbed only through the
  media rate (300 kbps → 1.1 Mbps in 5 s).
- With `set_desired_bitrate(2 Mbps)`, probes took it to about 3.8 Mbps.
- With a 250 kbps bottleneck imposed at t = 15 s, the smoothed estimate
  fell from 1.35 Mbps to 908 kbps at +1.4 s, 310 kbps at +2.1 s and
  168 kbps at +6 s. It then recovered slowly through AIMD, to 330 kbps at
  +13 s.

**Verdict.** str0m 0.24.1 provides usable **per-peer** egress BWE. The SFU
holds one `Rtc` per participant, so one estimate per `Rtc` is exactly one
estimate **per receiver**. Splitting that estimate across the origins a
receiver subscribes to, and turning it into layers, is entirely huddle's
job. If a peer does not negotiate TWCC, `can_probe` stays false and no
`Twcc` estimate ever arrives. The fallback for that case is defined below.

## Decision

### D1. Enable str0m BWE on every peer; one allocator per receiver

`Peer::new` adds `.enable_bwe(Some(initial))`. `initial` is
`Config::simulcast_initial_bwe_kbps`, default **1000 kbps**. That covers
one remote high layer plus audio from the first frame, so the two-person
case from #111 is high immediately. The pacer then starts at 2 Mbps and
settles at 1.1 × the estimate.

Each `Peer` owns a `LayerAllocator`, a new pure module `crates/sfu/src/alloc.rs`
with no str0m `Rtc` and no clock reads. The peer feeds it inputs and
applies its outputs:

```rust
pub(crate) struct LayerAllocator { /* per-receiver state, see D4 */ }

impl LayerAllocator {
    fn on_estimate(&mut self, now: Instant, estimate: Bitrate, can_probe: bool);
    fn set_focus(&mut self, now: Instant, origin: Option<PeerId>);
    fn set_sources(&mut self, now: Instant, sources: &[Source]); // tracks_out snapshot
    fn tick(&mut self, now: Instant) -> Vec<Decision>;           // called from handle_timeout
    fn desired_bitrate(&self) -> Bitrate;
}
pub(crate) struct Source { origin: PeerId, kind: SourceKind, joined_seq: u64, live_rids: LayerSet }
pub(crate) struct Decision { origin: PeerId, layer: VideoLayer, reason: SwitchReason }
pub(crate) enum SwitchReason { Bwe, Recovery, Focus, Unavailable }
```

`Peer::handle_event` stops ignoring three events:

- `Event::EgressBitrateEstimate(BweKind::Twcc { estimate, can_probe })` →
  `alloc.on_estimate(now, estimate, can_probe)`.
- `Event::MediaIngressStats`, on the **origin** peer → per-(mid, rid)
  liveness and measured rate (D2, D6).
- `BweKind::Remb` → ignored.

After every allocator change, `rtc.bwe().set_desired_bitrate(alloc.desired_bitrate())`.
The desired bitrate is the reserve (D3) plus the High cost of every
subscribed camera, times 1.1. This keeps the probe controller hunting for
enough capacity to re-upgrade everyone.

**Sans-io seam.** `Room::set_egress_estimate_override(peer, Option<Bitrate>)`
pins the allocator's estimate input. While set, it wins over str0m's
events. `now` always comes in from `Room::handle_timeout`. The server
exposes the seam only through the test/diagnostic config knob
`Config::simulcast_bwe_override_kbps` (`HUDDLE_SIMULCAST_BWE_OVERRIDE_KBPS`).
The knob is documented as not for production. It is what lets e2e assert
decisions without depending on real BWE (D9).

### D2. Layer costs: configured budgets, with measurement used only for liveness

For v1, a layer's cost is the **sender's configured budget**:
`High = 500 kbps` and `Low = 100 kbps`, mirrored as constants in
`crates/sfu/src/alloc.rs` with a comment that ties them to `publish_camera`
in `browser.rs`. These are the caps the browser encoder works up to under
motion, so allocating against them never overcommits. They are also
deterministic, which keeps the tests simple. If the budgets in
`browser.rs` change, the constants change in the same PR.

Measured per-rid ingress rates (bytes deltas of the origin's
`MediaIngressStats`) are **not** used as costs in v1. Low-motion senders
undershoot their caps, so measured costs would admit more High layers.
That refinement waits until the #115 metrics show the gap is worth the
nondeterminism.

v1 does use measurement for **liveness**. A rid is *live* if a frame
arrived on it in the last **1.5 s**: `TrackIn` records the last-frame
`Instant` per rid in `route_media`. Chrome's sender BWE switches off the
top encoding when the *sender's* uplink is constrained (the #78 "stalled
layer"). Defaulting to High would otherwise freeze tiles. A non-live
target layer falls back to the other live layer (`SwitchReason::Unavailable`).
When it becomes live again, the allocator re-evaluates without any hold.

### D3. Budget and per-receiver allocation across origins

Inputs at time `now` for one receiver:

- `E`: the effective estimate. This is the override if set. Otherwise it is
  the last `Twcc.estimate`, or `initial` before the first event.
- `R`: the non-adaptive reserve. It is 40 kbps per forwarded audio track,
  plus, for each live screen share, its measured ingress rate (its
  `MediaIngressStats`) or **1500 kbps** until a measurement exists. Screens
  and audio are never layer-filtered, so the allocator plans around them.
- The **keep budget** `B_keep = 0.90 · E − R`.
- The **upgrade budget** `B_up = 0.90 · E / 1.25 − R`. This adds 25 %
  headroom on top of the keep threshold and forms the hysteresis band.

Allocation is greedy in **priority order**:

1. The focused origin (D5), if any.
2. Origins that hold High right now (stickiness). Unfocusing or a
   rebalance does not reshuffle stable allocations.
3. Join order (`joined_seq`, a monotonic counter assigned when the
   `TrackOut` is created). This order is stable and deterministic.

Steps:

1. Every camera origin starts from Low. Its cost is `Σ Low`. There is no
   "off" or paused state in v1. A receiver that cannot afford even
   `Σ Low` keeps Low everywhere. Pausing video is a later extension.
2. Walk the priority order and upgrade origin `o` to High while
   `Σ cost ≤ budget`, where `budget` is
   - `B_keep` if `o` holds High now, or the move is budget-neutral (a focus
     swap), or this is the initial allocation;
   - `B_up` otherwise, together with the hold of D4.
3. Apply liveness (D2) last: a High pick whose high rid is not live becomes
   Low, and vice versa.

**Initial allocation is "default High".** A new subscription is placed at
High, and the first `tick` runs step 2 with `B_keep` across all origins.
In a two-person call, `E = 1000 kbps` gives `B_keep = 900 − 40 = 860`,
which is ≥ 500, so the result is High with no client signal at all.

### D4. Per-receiver state machine and anti-flap

The allocator keeps, per receiver:

```text
            estimate drop: allocation(B_keep) < current           (immediate)
   ┌────────────────────────────────────────────────────────────────┐
   ▼                                                                │
 STABLE ──(allocation(B_up) > current)──▶ UPGRADE_PENDING{since} ───┘
   ▲                                          │ held ≥ hold, still fits B_up
   │                                          ▼
   └──────────────── apply upgrade, enter STABLE (last_change = now)
```

- **Downshift is immediate** on the first smoothed estimate whose
  allocation under `B_keep` is lower than the current one. str0m's 3 s
  smoother already filters spikes (spike: a real collapse is visible at
  about +1.5 s). Delaying further only prolongs congestion. Downshifts
  drop the lowest-priority High first.
- **An upgrade needs `B_up` to hold continuously for `hold`.** `hold`
  starts at **5 s** after a change. Any estimate that breaks `B_up` during
  the hold cancels the pending upgrade.
- **Backoff stops flapping.** A downshift within **30 s** of an upgrade
  doubles `hold`: 5 → 10 → 20 → 40 s, capped at 40 s. `hold` resets to 5 s
  after **60 s** without a downshift.
- **`can_probe: false`, or no `Twcc` event ever:** freeze. Keep the current
  allocation, which starts at default High, and only react to focus and
  liveness. This is the **fallback for peers without TWCC**. It behaves
  like today's non-adaptive forwarding, without the low default. An
  RR-loss-based (`MediaEgressStats::loss`) downshift is deferred until a
  real non-TWCC client appears.
- **Stale estimate:** the `Twcc` event is edge-triggered. Silence means
  "unchanged", not "unknown", so there is no staleness timeout.

Each upgrade is a single step. When several origins qualify, only the
highest-priority one is upgraded per hold, so each probe-backed increase is
confirmed before the next one.

### D5. Focus as tiebreaker, and the protocol shape for the hint

Focus is one per-receiver `Option<PeerId>` held by the allocator. It
**reorders priority**. It never forces a layer the budget cannot carry.
A focus change re-runs allocation immediately. Swapping the High slot
from A to the newly focused B is budget-neutral, so it uses `B_keep` and
skips the hold. Upgrading the focused origin into budget that was not in
use goes through the normal `B_up` and hold path.

Protocol (additive-only, `PROTOCOL_VERSION` stays 1, landing in its own
commit):

```rust
/// Hint: this receiver's focused (stage) participant, or none. The
/// server arbitrates layers; focus only wins ties under constraint.
ClientMessage::Focus { origin: Option<ParticipantId> }  // {"type":"focus","origin":…|null}
```

Backward compatibility for `ClientMessage::LayerPreference { origin, layer }`:
the variant stays and the server keeps accepting it as a legacy alias.

- `High` maps to `set_focus(Some(origin))`.
- `Low` for the currently focused origin maps to `set_focus(None)`.
- Any other `Low` is a **no-op**. It is no longer a cap.

An old client that sends Low for every unfocused tile therefore gets
High-by-default under no constraint, which is the #111 fix, and still
gets "focused wins" under constraint. A future viewport cap (an
off-screen or tiny tile asking for at most Low) would be a new additive
variant. It must not reuse `Low`.

### D6. Glitch-free switches

When a decision changes the target layer, `TrackOut` gets
`pending: Option<(VideoLayer, since)>`. The SFU asks the origin for a FIR
on the target rid, using the existing per-rid throttle, and **keeps
forwarding the current rid until a keyframe (`MediaData::is_keyframe()`)
arrives on the target rid**. It then cuts over. If no keyframe arrives
within **1 s**, it cuts over anyway, which is today's behaviour. If the
current rid dies while a switch is pending, it cuts over at once.
`huddle_peer_simulcast_layer` reports the **forwarded** layer.

### D7. Metrics (#115), with low cardinality

These reuse the existing per-peer label set (`room, peer, id`), which is
already bounded by `max_participants` × live rooms:

- `huddle_peer_egress_estimate_bps` (gauge): `E` as the allocator sees it,
  override included.
- `huddle_peer_egress_desired_bps` (gauge): what was passed to
  `set_desired_bitrate`.
- `huddle_peer_bwe_can_probe` (gauge, 0/1): a stuck 0 means TWCC was not
  negotiated (D4 fallback).
- `huddle_peer_layer_upgrade_hold_seconds` (gauge): the current backoff
  `hold`. This is the hysteresis state.
- `huddle_peer_simulcast_layer{…,origin}` is **unchanged** in name and
  encoding. It now reports the forwarded layer, as in D6.
- Server-wide counter, with no peer labels:
  `huddle_simulcast_layer_switches_total{direction="up|down",reason="bwe|recovery|focus|unavailable"}`,
  at most 8 series.

"Last transition + reason" is a **log**, not a metric (a reason label per
peer would multiply series). Each switch emits one structured `info!` with
`room, receiver, origin, from, to, reason, estimate_bps, budget_bps`.

### D8. What huddle must compute itself (gaps in str0m)

str0m provides one smoothed per-transport estimate, `can_probe`, desired
bitrate and probing, pacing, and per-(mid, rid) stats. Everything else
belongs to huddle:

1. Splitting the per-receiver estimate across origins, and the priority
   order (D3, D5).
2. Layer cost model and constants (D2).
3. Non-adaptive reserve for audio and screens (D3).
4. Upgrade and downshift thresholds, hold, backoff, freeze (D4).
5. Desired bitrate, so probing can find re-upgrade capacity (D1).
6. Per-rid liveness, so the SFU never parks a receiver on a stalled layer
   (D2).
7. Keyframe-gated cut-over (D6). str0m forwards whatever is written.
8. The override seam and its config knob (D1, D9).
9. Metrics and logs (D7).

### D9. Deterministic test strategy (#116)

1. **Allocator unit tests** (`crates/sfu/src/alloc.rs`, pure). Synthetic
   `Instant` (`t0 + n·ms`) and `Bitrate` sequences cover:
   - default High
   - collapse → immediate downshift of the lowest-priority origin first
   - recovery → no upgrade before the hold expires, upgrade exactly at it
   - flapping (estimate oscillating ±20 % around `B_up`) → at most 1 switch
     per hold, with backoff doubling and resetting
   - `can_probe: false` freeze
   - focus swap being budget-neutral and immediate
   - unfocus not reshuffling (stickiness)
   - liveness fallback
   - desired-bitrate arithmetic

   No wall clock and no str0m.
2. **Room sans-io** (`crates/sfu/tests/sans_io.rs`). Existing synthetic
   clients negotiate TWCC by default, so real estimates would race the
   test. These tests therefore always set
   `Room::set_egress_estimate_override`. They assert routed layers with the
   existing per-rid marker bytes and the D6 keyframe gate (no frames of the
   new rid before its keyframe). They also assert that `LayerPreference`
   High/Low still maps to focus and no-op. The existing
   `simulcast_routes_the_subscribers_chosen_layer` flips its default
   expectation from low to high.
3. **e2e** (`e2e-call.mjs`). Boot with
   `HUDDLE_SIMULCAST_BWE_OVERRIDE_KBPS=900`. One High (500) plus two audio
   streams (80) is 580 ≤ `B_keep` (810). Two High plus two audio is
   1080 > 810, so exactly one of two remote cameras is High. All
   assertions stay on `huddle_peer_simulcast_layer` (the decision), with
   frame-width checks still gated on sender liveness:
   - Two people: bob ← alice is **high with no focus** (the #111
     regression).
   - With a third participant: exactly one origin is high. Focusing the
     low one swaps it. Unfocusing changes nothing.
   - No assertion depends on real BWE timing.

### D10. PR sequence

| # | Issue | Content |
|---|---|---|
| 1 | #112 | This ADR. |
| 2 | #113 | `alloc.rs` with unit tests (#116 part 1); `enable_bwe` + event wiring + `set_desired_bitrate`; default High; per-rid liveness; D6 keyframe gate; override seam + `simulcast_*` config; legacy `LayerPreference` mapping (High → focus, Low → no-op); sans-io updates; e2e flipped to the default-High expectation under the override. Must keep the existing metric green. |
| 3 | #115 | The D7 gauges, counter and switch log, plus render tests in `metrics.rs`. |
| 4 | #114 | Protocol commit alone (`ClientMessage::Focus` + round-trip test), then server handling, then the client sending `Focus` from `UiAction::Focus`. Three-party e2e tiebreak assertion (#116 part 2). |
| 5 | #117 | Client cleanup: drop the per-participant layer loop and `set_layer_preference` from `call.rs`/`rtc/mod.rs`; update `mock_server.rs`; fix the stale "three simulcast layers" doc in `attach_stream`. The server keeps the legacy alias. |
| 6 | #116 | Remaining matrix: flap/backoff sequences, metrics assertions in e2e; closes the story. |

## Consequences

**Positive**
- Two-person and small calls get the high layer with no client
  involvement. Constrained receivers degrade per origin instead of
  freezing.
- Focus keeps its meaning (who wins under constraint) and old clients
  keep working, all within protocol v1.
- The decision loop is a pure, clock-injected state machine. Every
  threshold is unit-testable, and e2e asserts decisions instead of
  network timing (the #78/#93 lesson).

**Negative / costs**
- **The pacer changes behaviour for all forwarding.** `enable_bwe` swaps the
  null pacer for a leaky bucket at 1.1 × the estimate. Over-allocation now
  shows up as queueing delay or `WriteWithoutPoll` sheds
  (`PeerStats::dropped_frames`) instead of wire loss. Probe padding
  (≥ 50 kbps while media is active) adds egress. #113 must watch
  `dropped_frames` (not yet exported to `/metrics`; #115 should add
  `huddle_peer_dropped_frames_total`), RTT and the e2e suites for
  regressions.
- Configured-budget costs are conservative. Low-motion senders could
  afford more High layers than the allocator grants until measured costs
  arrive.
- Reaction time is bounded by str0m's 3 s smoother: a real collapse
  downshifts in roughly 1.5–2 s. Re-upgrade takes at least 5 s and up to
  40 s under backoff.
- Server-side decisions cannot reach the sender's encoder (no
  `setParameters` with E2EE). Layers nobody watches are still encoded and
  uploaded. That is the existing cost and is out of scope.
- A test/diagnostic config knob (`simulcast_bwe_override_kbps`) ships in
  production config. It is documented as such.

**Rejected alternatives**
- *Client-side policy with focus only (status quo)*: no bandwidth input,
  low by default. This is the reported bug.
- *Unsmoothed `PeerStats::bwe_tx` as the input*: periodic and faster, but
  it is the raw GoogCC value with probe overshoots and AIMD sawtooth, so
  flapping would need its own filter. The smoothed event is the value str0m
  itself recommends allocating against.
- *Per-origin REMB*: browsers do not send REMB when transport-cc is
  negotiated. It would also only cap one m-line, not split a receiver's
  link.
- *Loss-only (RR) adaptation as the primary signal*: it reacts only after
  damage and gives no capacity estimate for re-upgrade. It is kept as the
  possible non-TWCC fallback.
- *A new `layer_cap` semantics for `LayerPreference::Low`*: this would
  silently keep old clients on Low, which is the bug.
