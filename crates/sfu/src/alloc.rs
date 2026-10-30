//! Per-receiver simulcast layer allocation (ADR 015).
//!
//! One [`LayerAllocator`] per receiving peer turns str0m's egress
//! bandwidth estimate for that receiver into a layer per origin camera.
//! It is pure: no `Rtc`, no clock reads — every input, `now` included,
//! is handed in, so the state machine is unit-testable with synthetic
//! estimate sequences.
//!
//! The model (ADR 015, D2–D5):
//!
//! * every camera costs its sender's configured budget — [`HIGH_COST`]
//!   or [`LOW_COST`] — and audio/screens are a fixed reserve;
//! * a receiver keeps `k` High layers while they fit 90 % of the
//!   estimate (the *keep* budget) and gains one more only while it fits
//!   with another 25 % of headroom (the *upgrade* budget), continuously
//!   for a hold that backs off when upgrades keep failing;
//! * which origins hold the High slots is a priority order: focus first,
//!   then whoever holds High already (stickiness), then join order;
//! * a layer whose rid stopped delivering frames is swapped for the
//!   other, live one.

use std::time::{Duration, Instant};

use huddle_protocol::VideoLayer;
use str0m::bwe::Bitrate;

use crate::PeerId;

/// Cost of an origin's high layer: the client's `max_bitrate` for the
/// `high` encoding in `crates/web/src/rtc/browser.rs` (`publish_camera`).
/// The two must change together.
pub(crate) const HIGH_COST: Bitrate = Bitrate::kbps(1_500);
/// Cost of an origin's low layer: the client's `low` encoding cap.
pub(crate) const LOW_COST: Bitrate = Bitrate::kbps(100);
/// Reserve per forwarded audio track (opus, never layer-filtered).
pub(crate) const AUDIO_RESERVE: Bitrate = Bitrate::kbps(40);
/// Reserve per screen share until its ingress rate has been measured.
pub(crate) const SCREEN_RESERVE: Bitrate = Bitrate::kbps(1_500);

/// Share of the estimate the allocation may fill (keep budget).
const KEEP_UTILIZATION: f64 = 0.90;
/// Extra headroom an upgrade needs on top of the keep budget.
const UPGRADE_HEADROOM: f64 = 1.25;
/// The desired bitrate handed to str0m's probe controller, relative to
/// the receiver's full-quality need.
const DESIRED_HEADROOM: f64 = 1.1;
/// How long the upgrade budget must hold before one more High is granted.
const HOLD_INITIAL: Duration = Duration::from_secs(5);
/// Ceiling of the backed-off hold.
const HOLD_MAX: Duration = Duration::from_secs(40);
/// A downshift this soon after an upgrade doubles the hold.
const BACKOFF_WINDOW: Duration = Duration::from_secs(30);
/// This long without a downshift resets the hold.
const HOLD_RESET: Duration = Duration::from_secs(60);

/// Why a receiver's layer for an origin changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchReason {
    /// The estimate no longer carries the allocation.
    Bwe,
    /// The estimate recovered and held the upgrade budget long enough.
    Recovery,
    /// The focus moved the High slot to another origin.
    Focus,
    /// The chosen layer's rid stopped (or resumed) delivering frames.
    Unavailable,
}

/// One simulcast camera the receiver subscribes to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Source {
    pub origin: PeerId,
    /// Subscription order (lower = earlier): the last tiebreaker.
    pub order: u64,
    /// Whether the high / low rid delivered a frame recently.
    pub high_live: bool,
    pub low_live: bool,
}

/// A layer change for one origin, to be applied by the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Decision {
    pub origin: PeerId,
    pub layer: VideoLayer,
    pub reason: SwitchReason,
}

/// The per-receiver allocation state machine (ADR 015, D4).
#[derive(Debug)]
pub(crate) struct LayerAllocator {
    /// The last `BweKind::Twcc` estimate and its `can_probe` flag.
    estimate: Option<(Bitrate, bool)>,
    /// A pinned estimate (tests and diagnostics); wins over `estimate`.
    estimate_override: Option<Bitrate>,
    focus: Option<PeerId>,
    /// Every origin seen so far (new ones start on High).
    known: Vec<PeerId>,
    /// Origins the budget grants High, before liveness.
    granted: Vec<PeerId>,
    /// The layer last reported per origin, after liveness. A new origin
    /// enters as High — the peer opens every subscription on High — so
    /// only a departure from that default is ever reported.
    reported: Vec<(PeerId, VideoLayer)>,
    /// Total camera cost of the previous round's grants (bps). Anything
    /// that fits it again is budget-neutral and needs no hold.
    allocated: f64,
    hold: Duration,
    /// Since when the upgrade budget has held, if it does.
    upgrade_since: Option<Instant>,
    last_upgrade: Option<Instant>,
    last_downshift: Option<Instant>,
}

impl Default for LayerAllocator {
    fn default() -> Self {
        Self {
            estimate: None,
            estimate_override: None,
            focus: None,
            known: Vec::new(),
            granted: Vec::new(),
            reported: Vec::new(),
            allocated: 0.0,
            hold: HOLD_INITIAL,
            upgrade_since: None,
            last_upgrade: None,
            last_downshift: None,
        }
    }
}

impl LayerAllocator {
    /// Record a `BweKind::Twcc` estimate. `can_probe == false` freezes
    /// the allocation until probing is available again.
    pub(crate) fn on_estimate(&mut self, estimate: Bitrate, can_probe: bool) {
        self.estimate = Some((estimate, can_probe));
    }

    /// Pin (or release) the estimate the allocation runs on.
    pub(crate) fn set_override(&mut self, estimate: Option<Bitrate>) {
        self.estimate_override = estimate;
    }

    /// Set the receiver's focused origin.
    pub(crate) fn set_focus(&mut self, focus: Option<PeerId>) {
        self.focus = focus;
    }

    pub(crate) fn focus(&self) -> Option<PeerId> {
        self.focus
    }

    /// The layer last reported for `origin`.
    pub(crate) fn layer(&self, origin: PeerId) -> Option<VideoLayer> {
        self.reported
            .iter()
            .find(|(o, _)| *o == origin)
            .map(|(_, l)| *l)
    }

    /// The estimate allocation runs on, or `None` while frozen (no
    /// estimate yet, or str0m cannot probe — e.g. no TWCC negotiated).
    pub(crate) fn effective_estimate(&self) -> Option<Bitrate> {
        self.estimate_override.or(match self.estimate {
            Some((estimate, true)) => Some(estimate),
            _ => None,
        })
    }

    /// The desired bitrate for str0m's probe controller: enough to carry
    /// every source on High plus the reserve, so probing can discover the
    /// capacity a re-upgrade needs.
    pub(crate) fn desired_bitrate(sources: usize, reserve: Bitrate) -> Bitrate {
        (reserve + HIGH_COST * sources as f64) * DESIRED_HEADROOM
    }

    /// Run one allocation round. `sources` is the receiver's current set
    /// of simulcast cameras, `reserve` the non-adaptive egress (audio,
    /// screens, single-layer cameras). Returns the layer changes since
    /// the previous round; a new origin is assumed to start on High and
    /// is only reported if it does not stay there.
    pub(crate) fn evaluate(
        &mut self,
        now: Instant,
        sources: &[Source],
        reserve: Bitrate,
    ) -> Vec<Decision> {
        let present = |o: &PeerId| sources.iter().any(|s| s.origin == *o);
        self.known.retain(present);
        self.granted.retain(present);
        self.reported.retain(|(o, _)| present(o));
        // Default High: a new subscription starts granted.
        for s in sources {
            if !self.known.contains(&s.origin) {
                self.known.push(s.origin);
                self.granted.push(s.origin);
                self.reported.push((s.origin, VideoLayer::High));
            }
        }
        if self
            .last_downshift
            .is_some_and(|t| now.saturating_duration_since(t) >= HOLD_RESET)
        {
            self.hold = HOLD_INITIAL;
        }

        let current = self.granted.len();
        let (count, reason) = match self.effective_estimate() {
            None => {
                self.upgrade_since = None;
                (current, SwitchReason::Focus)
            }
            Some(estimate) => self.target_count(now, estimate, sources.len(), reserve),
        };

        // Priority: focus, then stickiness, then subscription order.
        let mut order: Vec<&Source> = sources.iter().collect();
        order.sort_by_key(|s| {
            (
                self.focus != Some(s.origin),
                !self.granted.contains(&s.origin),
                s.order,
            )
        });
        let granted: Vec<PeerId> = order.iter().take(count).map(|s| s.origin).collect();
        self.allocated = cost(granted.len(), sources.len());
        let changed: Vec<PeerId> = sources
            .iter()
            .map(|s| s.origin)
            .filter(|o| granted.contains(o) != self.granted.contains(o))
            .collect();
        self.granted = granted;

        let mut decisions = Vec::new();
        for s in sources {
            let intended = if self.granted.contains(&s.origin) {
                VideoLayer::High
            } else {
                VideoLayer::Low
            };
            let (layer, unavailable) = match intended {
                VideoLayer::High if !s.high_live && s.low_live => (VideoLayer::Low, true),
                VideoLayer::Low if !s.low_live && s.high_live => (VideoLayer::High, true),
                layer => (layer, false),
            };
            if self.layer(s.origin) == Some(layer) {
                continue;
            }
            let reason = if unavailable || !changed.contains(&s.origin) {
                // Only liveness moved it (away, or back to the intended layer).
                SwitchReason::Unavailable
            } else {
                reason
            };
            match self.reported.iter_mut().find(|(o, _)| *o == s.origin) {
                Some(entry) => entry.1 = layer,
                None => self.reported.push((s.origin, layer)),
            }
            decisions.push(Decision {
                origin: s.origin,
                layer,
                reason,
            });
        }
        decisions
    }

    /// How many High slots the receiver should hold after this round,
    /// and the reason when that differs from now. Applies the
    /// downshift / hold / backoff rules of ADR 015, D4.
    fn target_count(
        &mut self,
        now: Instant,
        estimate: Bitrate,
        n: usize,
        reserve: Bitrate,
    ) -> (usize, SwitchReason) {
        let current = self.granted.len();
        let keep = slots(estimate.as_f64() * KEEP_UTILIZATION - reserve.as_f64(), n);
        let up = slots(
            estimate.as_f64() * KEEP_UTILIZATION / UPGRADE_HEADROOM - reserve.as_f64(),
            n,
        );
        if current > keep {
            self.upgrade_since = None;
            if self
                .last_upgrade
                .is_some_and(|t| now.saturating_duration_since(t) < BACKOFF_WINDOW)
            {
                self.hold = (self.hold * 2).min(HOLD_MAX);
            }
            self.last_downshift = Some(now);
            return (keep, SwitchReason::Bwe);
        }
        // Budget freed elsewhere (an origin left): taking it back up to
        // the previous total is not an upgrade of the link.
        let neutral = (current..=keep)
            .rev()
            .find(|k| cost(*k, n) <= self.allocated)
            .unwrap_or(current);
        if neutral > current {
            self.upgrade_since = None;
            return (neutral, SwitchReason::Recovery);
        }
        if current >= up {
            // Inside the hysteresis band: hold still, cancel any pending upgrade.
            self.upgrade_since = None;
            return (current, SwitchReason::Focus);
        }
        let since = *self.upgrade_since.get_or_insert(now);
        if now.saturating_duration_since(since) < self.hold {
            return (current, SwitchReason::Focus);
        }
        // One upgrade per hold: the next one starts its own hold.
        self.upgrade_since = None;
        self.last_upgrade = Some(now);
        (current + 1, SwitchReason::Recovery)
    }
}

/// Camera cost (bps) of `high` of `n` cameras on High, the rest on Low.
fn cost(high: usize, n: usize) -> f64 {
    LOW_COST.as_f64() * n as f64 + (HIGH_COST.as_f64() - LOW_COST.as_f64()) * high as f64
}

/// How many of `n` cameras can be on High within `budget_bps`, all
/// others on Low.
fn slots(budget_bps: f64, n: usize) -> usize {
    let base = LOW_COST.as_f64() * n as f64;
    if budget_bps < base {
        return 0;
    }
    let step = HIGH_COST.as_f64() - LOW_COST.as_f64();
    (((budget_bps - base) / step).floor() as usize).min(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two audio tracks, as a receiver of two remote participants has.
    const RESERVE: Bitrate = Bitrate::kbps(80);

    fn origin(n: u128) -> PeerId {
        PeerId::from_u128(n)
    }

    fn live(n: u128) -> Source {
        Source {
            origin: origin(n),
            order: n as u64,
            high_live: true,
            low_live: true,
        }
    }

    fn at(t0: Instant, secs: f64) -> Instant {
        t0 + Duration::from_secs_f64(secs)
    }

    fn decision(n: u128, layer: VideoLayer, reason: SwitchReason) -> Decision {
        Decision {
            origin: origin(n),
            layer,
            reason,
        }
    }

    /// Feed a piecewise-constant estimate: each `(from_secs, kbps)` holds
    /// until the next step's `from_secs` (the last step for one sample),
    /// evaluated every 100 ms as `Room::handle_timeout` does. Returns
    /// every decision with the time (in 100 ms ticks) it was made.
    fn run(
        alloc: &mut LayerAllocator,
        t0: Instant,
        sources: &[Source],
        steps: &[(u64, u64)],
    ) -> Vec<(u64, Decision)> {
        let mut log = Vec::new();
        for (i, &(from, kbps)) in steps.iter().enumerate() {
            let until = steps.get(i + 1).map_or(from * 10 + 1, |s| s.0 * 10);
            alloc.on_estimate(Bitrate::kbps(kbps), true);
            for tick in from * 10..until {
                let now = at(t0, tick as f64 / 10.0);
                for d in alloc.evaluate(now, sources, RESERVE) {
                    log.push((tick, d));
                }
            }
        }
        log
    }

    fn with_reason(log: &[(u64, Decision)], reason: SwitchReason) -> Vec<(u64, Decision)> {
        log.iter()
            .copied()
            .filter(|(_, d)| d.reason == reason)
            .collect()
    }

    #[test]
    fn new_sources_default_to_high_without_any_estimate() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        let log = alloc.evaluate(t0, &[live(1), live(2), live(3)], RESERVE);
        assert!(log.is_empty(), "the default needs no switch: {log:?}");
        for n in 1..=3 {
            assert_eq!(alloc.layer(origin(n)), Some(VideoLayer::High));
        }
    }

    #[test]
    fn initial_allocation_fits_the_keep_budget() {
        let t0 = Instant::now();
        // Two-person call: the default 2 Mbps initial estimate carries High.
        let mut alloc = LayerAllocator::default();
        alloc.on_estimate(Bitrate::kbps(2_000), true);
        assert!(alloc.evaluate(t0, &[live(1)], Bitrate::kbps(40)).is_empty());
        assert_eq!(alloc.layer(origin(1)), Some(VideoLayer::High));

        // Three-person call: one High fits, the later subscription drops.
        let mut alloc = LayerAllocator::default();
        alloc.on_estimate(Bitrate::kbps(2_000), true);
        assert_eq!(
            alloc.evaluate(t0, &[live(1), live(2)], RESERVE),
            vec![decision(2, VideoLayer::Low, SwitchReason::Bwe)]
        );
    }

    #[test]
    fn collapse_downshifts_immediately_lowest_priority_first() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        let sources = [live(1), live(2)];
        // 5 Mbps carries both on High; at t=10 s the link collapses to
        // 2 Mbps: one High still fits and the later origin drops — on
        // the very first sample of the new estimate.
        let log = run(&mut alloc, t0, &sources, &[(0, 5_000), (10, 2_000)]);
        assert_eq!(
            log,
            vec![(100, decision(2, VideoLayer::Low, SwitchReason::Bwe))]
        );

        // A deeper collapse takes the last High too.
        let log = run(&mut alloc, t0, &sources, &[(20, 500)]);
        assert_eq!(
            log,
            vec![(200, decision(1, VideoLayer::Low, SwitchReason::Bwe))]
        );
    }

    #[test]
    fn recovery_upgrades_exactly_when_the_hold_expires() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        let sources = [live(1), live(2)];
        // Downshift at t=0, plenty from t=10 s: the 5 s hold ends at 15 s.
        let log = run(
            &mut alloc,
            t0,
            &sources,
            &[(0, 2_000), (10, 5_000), (20, 5_000)],
        );
        assert_eq!(
            with_reason(&log, SwitchReason::Recovery),
            vec![(150, decision(2, VideoLayer::High, SwitchReason::Recovery))]
        );
    }

    #[test]
    fn one_upgrade_per_hold_in_priority_order() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        let sources = [live(1), live(2), live(3)];
        // Starved (all Low) at t=0, then plenty from t=1 s: each further
        // High waits out its own 5 s hold.
        let log = run(
            &mut alloc,
            t0,
            &sources,
            &[(0, 300), (1, 20_000), (30, 20_000)],
        );
        let ups = with_reason(&log, SwitchReason::Recovery);
        assert_eq!(
            ups.iter().map(|(_, d)| d.origin).collect::<Vec<_>>(),
            vec![origin(1), origin(2), origin(3)],
            "{log:?}"
        );
        for pair in ups.windows(2) {
            assert!(pair[1].0 - pair[0].0 >= 50, "closer than a hold: {ups:?}");
        }
    }

    #[test]
    fn rapid_oscillation_never_upgrades() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        let sources = [live(1), live(2)];
        // The classic flap: the estimate alternates every second between
        // clearing the upgrade budget (two High need E ≥ 4278 kbps) and
        // sitting inside the band. The hold never completes.
        let mut steps = vec![(0, 2_000)];
        for t in 1..60 {
            steps.push((t, if t % 2 == 1 { 5_000 } else { 4_000 }));
        }
        let log = run(&mut alloc, t0, &sources, &steps);
        assert_eq!(
            log,
            vec![(0, decision(2, VideoLayer::Low, SwitchReason::Bwe))],
            "flapped"
        );
    }

    #[test]
    fn failed_upgrades_back_off_to_the_cap_and_reset() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        let sources = [live(1), live(2)];
        // Each cycle the estimate clears the upgrade budget just long
        // enough for the (growing) hold, then dips below the keep budget
        // (two High break below 3423 kbps) two seconds after the upgrade.
        let mut steps = vec![(0, 2_000)];
        let mut t = 1;
        let mut holds = Vec::new();
        for hold in [5, 10, 20, 40, 40] {
            holds.push(hold);
            steps.push((t, 5_000));
            t += hold + 2;
            steps.push((t, 3_300));
            t += 1;
        }
        steps.push((t, 3_300));
        let log = run(&mut alloc, t0, &sources, &steps);
        let ups = with_reason(&log, SwitchReason::Recovery);
        assert_eq!(ups.len(), holds.len(), "one upgrade per cycle: {log:?}");
        assert_eq!(with_reason(&log, SwitchReason::Bwe).len(), holds.len() + 1);
        assert_eq!(alloc.hold, HOLD_MAX);

        // A minute without a downshift resets the hold.
        let log = run(&mut alloc, t0, &sources, &[(t + 1, 3_300), (t + 62, 3_300)]);
        assert!(log.is_empty(), "{log:?}");
        assert_eq!(alloc.hold, HOLD_INITIAL);
    }

    #[test]
    fn focus_swaps_the_high_slot_immediately_and_unfocus_keeps_it() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        let sources = [live(1), live(2)];
        // The e2e override: one High + one Low fit, two High do not.
        alloc.set_override(Some(Bitrate::kbps(2_500)));
        assert_eq!(
            alloc.evaluate(t0, &sources, RESERVE),
            vec![decision(2, VideoLayer::Low, SwitchReason::Bwe)]
        );

        // Budget-neutral swap: no hold, both sides switch in one round.
        alloc.set_focus(Some(origin(2)));
        assert_eq!(
            alloc.evaluate(at(t0, 0.1), &sources, RESERVE),
            vec![
                decision(1, VideoLayer::Low, SwitchReason::Focus),
                decision(2, VideoLayer::High, SwitchReason::Focus),
            ]
        );

        // Unfocus: stickiness keeps the slot where it is.
        alloc.set_focus(None);
        assert!(alloc.evaluate(at(t0, 0.2), &sources, RESERVE).is_empty());
    }

    #[test]
    fn focus_never_forces_high_past_the_budget() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        alloc.set_override(Some(Bitrate::kbps(500)));
        alloc.set_focus(Some(origin(2)));
        alloc.evaluate(t0, &[live(1), live(2)], RESERVE);
        assert_eq!(alloc.layer(origin(1)), Some(VideoLayer::Low));
        assert_eq!(alloc.layer(origin(2)), Some(VideoLayer::Low));
    }

    #[test]
    fn losing_probe_capability_freezes_the_allocation() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        let sources = [live(1), live(2)];
        alloc.on_estimate(Bitrate::kbps(5_000), true);
        assert!(alloc.evaluate(t0, &sources, RESERVE).is_empty());
        // str0m re-emits the last estimate with can_probe=false; even a
        // tiny value must not downshift while frozen, nor may focus.
        alloc.on_estimate(Bitrate::kbps(100), false);
        alloc.set_focus(Some(origin(2)));
        assert!(alloc.evaluate(at(t0, 1.0), &sources, RESERVE).is_empty());
        assert_eq!(alloc.effective_estimate(), None);
    }

    #[test]
    fn the_override_wins_over_str0m_estimates() {
        let mut alloc = LayerAllocator::default();
        alloc.set_override(Some(Bitrate::kbps(2_500)));
        alloc.on_estimate(Bitrate::kbps(100), true);
        assert_eq!(alloc.effective_estimate(), Some(Bitrate::kbps(2_500)));
        alloc.set_override(None);
        assert_eq!(alloc.effective_estimate(), Some(Bitrate::kbps(100)));
    }

    #[test]
    fn a_stalled_layer_falls_back_to_the_live_one() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        let mut sources = [live(1)];
        alloc.evaluate(t0, &sources, RESERVE);

        // The sender stops encoding its high layer (uplink pressure).
        sources[0].high_live = false;
        assert_eq!(
            alloc.evaluate(at(t0, 1.0), &sources, RESERVE),
            vec![decision(1, VideoLayer::Low, SwitchReason::Unavailable)]
        );

        // It resumes: straight back, no hold.
        sources[0].high_live = true;
        assert_eq!(
            alloc.evaluate(at(t0, 1.1), &sources, RESERVE),
            vec![decision(1, VideoLayer::High, SwitchReason::Unavailable)]
        );

        // Nothing live at all (a fresh subscription): keep the intent.
        sources[0].high_live = false;
        sources[0].low_live = false;
        assert!(alloc.evaluate(at(t0, 1.2), &sources, RESERVE).is_empty());
    }

    #[test]
    fn a_departure_hands_its_budget_on_without_a_hold() {
        let mut alloc = LayerAllocator::default();
        let t0 = Instant::now();
        alloc.set_override(Some(Bitrate::kbps(2_500)));
        alloc.evaluate(t0, &[live(1), live(2)], RESERVE);
        assert_eq!(alloc.layer(origin(2)), Some(VideoLayer::Low));
        // Origin 1 leaves: the receiver's total shrinks, so origin 2 can
        // take the freed High at once.
        assert_eq!(
            alloc.evaluate(at(t0, 0.1), &[live(2)], Bitrate::kbps(40)),
            vec![decision(2, VideoLayer::High, SwitchReason::Recovery)]
        );
        assert_eq!(alloc.layer(origin(1)), None, "departed origin forgotten");
    }

    #[test]
    fn desired_bitrate_covers_everyone_on_high() {
        let desired = LayerAllocator::desired_bitrate(2, RESERVE).as_f64();
        let expected = (80_000.0 + 2.0 * 1_500_000.0) * 1.1;
        assert!((desired - expected).abs() < 1.0, "{desired} vs {expected}");
    }
}
