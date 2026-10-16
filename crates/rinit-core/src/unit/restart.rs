//! Restart backoff and the start-limit circuit breaker (plan §1.3(4)).
//!
//! Without this, a service that crashes instantly is restarted in a tight loop
//! forever, which on a 128 MB device means both log spam and CPU burn. Two
//! mechanisms prevent it:
//!
//! - **Exponential backoff** with a cap and jitter. The cap stops the delay
//!   growing without bound; the jitter (`delay = base + Random(0, base/4)`,
//!   added only) keeps a fleet of services that all crashed at boot from
//!   coming back up in lockstep. Jitter is only ever added, never subtracted,
//!   shortening a delay could make it fire *earlier* than the base and
//!   re-introduce the synchronisation it exists to break up.
//! - **A circuit breaker** over a sliding window. After `StartLimitBurst`
//!   failures inside `StartLimitIntervalSec`, the unit is parked in
//!   `Failed(StartLimitHit)` and will not retry on its own.
//!
//! The failure counter resets only after the unit has stayed up for a whole
//! interval ("stable for a window counts as success", plan §1.3(4)). That is
//! deliberately stricter than systemd: a service that crashes every 30s with
//! `StartLimitIntervalSec=10` should keep accumulating, not quietly reset.

use alloc::collections::VecDeque;
use core::time::Duration;

use super::job::Millis;

/// The exponent cap on the backoff: `RestartSec * 2^5` (plan §1.3(4)).
///
/// With the default 2s base that is 64s, after which the delay stops growing.
const BACKOFF_EXPONENT_CAP: u32 = 5;

/// Convert a duration to whole milliseconds, saturating rather than wrapping.
fn as_millis(d: Duration) -> u64 {
    let secs = d.as_secs();
    let nanos = d.subsec_millis() as u64;
    secs.saturating_mul(1000).saturating_add(nanos)
}

/// A small xorshift generator.
///
/// The jitter has to be *cheap and non-cryptographic*, not random in any
/// statistical sense: its only job is to de-synchronise services that failed at
/// the same instant. Pulling in an RNG crate for that would cost more than it
/// buys, and a deterministic-but-varied sequence is enough. It is seeded per
/// unit by the caller (e.g. from the unit name), so two units do not step in
/// lockstep.
#[derive(Debug, Clone)]
pub struct Jitter(u64);

impl Jitter {
    /// Seed the generator. A zero seed is replaced, since xorshift cannot
    /// escape it.
    pub fn new(seed: u64) -> Self {
        Jitter(if seed == 0 { 0x9E3779B97F4A7C15 } else { seed })
    }

    /// The next value in `[0, 1)`, scaled to `bound`.
    ///
    /// Returns 0 when `bound` is 0, so a zero base adds no jitter instead of
    /// panicking.
    pub fn next_below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        // xorshift64*: fast, no dependencies, good enough for spreading
        // restarts.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D) % bound
    }
}

/// The restart bookkeeping for one unit (plan §1.3(4)).
#[derive(Debug, Clone)]
pub struct BackoffState {
    /// `RestartSec`, the base of the sequence.
    base: Duration,
    /// Consecutive restarts so far; the `n` of `base * 2^(n-1)`.
    consecutive: u32,
    /// Failures inside the sliding window.
    window: VecDeque<Millis>,
    /// Failures allowed per window before the breaker trips.
    burst: u32,
    /// Width of the sliding window.
    interval: Duration,
    /// When the unit last reached `Active`, so a stable run can reset `n`.
    became_active_at: Option<Millis>,
    /// Jitter source, seeded per unit.
    jitter: Jitter,
}

impl BackoffState {
    /// Create the bookkeeping for a unit.
    ///
    /// `seed` should differ per unit - hashing the unit name is the obvious
    /// choice - so that services failing together do not restart together.
    pub fn new(base: Duration, burst: u32, interval: Duration, seed: u64) -> Self {
        BackoffState {
            base,
            consecutive: 0,
            window: VecDeque::new(),
            burst,
            interval,
            became_active_at: None,
            jitter: Jitter::new(seed),
        }
    }

    /// Record that the unit reached `Active` at `now`.
    ///
    /// Only the timestamp is recorded. The reset decision happens in
    /// [`BackoffState::on_failure`], which is the only place that knows a
    /// failure actually occurred: resetting here too would double-count a
    /// window that both paths clear.
    pub fn on_active(&mut self, now: Millis) {
        self.became_active_at = Some(now);
    }

    /// Whether the unit has been up for at least one interval.
    fn has_been_stable_since(&self, now: Millis) -> bool {
        match self.became_active_at {
            // Plan §1.1: `StartLimitIntervalSec=0` disables the breaker
            // entirely, and with it this stability rule.
            None => false,
            // With the breaker disabled there is no window to be stable for.
            Some(_) if self.interval == Duration::ZERO => true,
            Some(at) => now.saturating_sub(at) >= as_millis(self.interval),
        }
    }

    /// Record a failure and decide whether to retry.
    ///
    /// Returns the delay to wait, or `None` if the breaker has tripped and the
    /// unit must stay in `Failed(StartLimitHit)`.
    ///
    /// The delay follows plan §1.3(4): `base * 2^(n-1)`, capped at
    /// `base * 2^5`, plus `Random(0, base/4)` of jitter.
    pub fn on_failure(&mut self, now: Millis) -> Option<Duration> {
        // A unit that had been up long enough resets before being counted,
        // otherwise a crash after a long healthy run would inherit an old
        // inflated count.
        if self.has_been_stable_since(now) {
            self.consecutive = 0;
            self.window.clear();
        }
        self.became_active_at = None;
        self.consecutive = self.consecutive.saturating_add(1);

        // Plan §1.1: `StartLimitIntervalSec=0` disables the breaker and
        // `StartLimitBurst=0` means no limit. Neither can ever read the window,
        // and with a zero interval nothing could expire out of it either, so
        // recording the failure here would grow the window forever - one
        // `Millis` per restart, without bound. (The M4 soak caught exactly
        // that.) When there is a breaker, `is_tripped` already answers `false`
        // for these configurations, so this is the same rule, applied where the
        // memory is spent.
        let width = as_millis(self.interval);
        if width == 0 || self.burst == 0 {
            return Some(self.next_delay());
        }

        self.window.push_back(now);
        // The window slides with the clock: failures older than `interval` have
        // left it. `now` is monotonic by construction (`UnitManager::now_millis`
        // counts from a fixed origin), so the expired entries are at the front,
        // which keeps this O(1) amortised instead of a scan of the whole window.
        while let Some(&front) = self.window.front() {
            if now.saturating_sub(front) >= width {
                self.window.pop_front();
            } else {
                break;
            }
        }
        // Only the newest `burst` failures can matter: that many is already a
        // tripped breaker, and the ones behind them can never be the reason it
        // trips. Without this, a long interval (or a high restart rate) would
        // let the window grow to `rate * interval` entries.
        while self.window.len() > self.burst as usize {
            self.window.pop_front();
        }

        if self.window.len() as u32 >= self.burst {
            None
        } else {
            Some(self.next_delay())
        }
    }

    /// The delay for the current attempt.
    fn next_delay(&mut self) -> Duration {
        let base_ms = as_millis(self.base);
        // `n` is 1-based: the first restart waits exactly `base`.
        let exponent = self.consecutive.saturating_sub(1).min(BACKOFF_EXPONENT_CAP);
        // Widen before multiplying so a large base saturates instead of
        // overflowing and wrapping to a *small* delay.
        let scaled = base_ms
            .checked_shl(exponent)
            .unwrap_or(u64::MAX)
            .min(base_ms.saturating_mul(1 << BACKOFF_EXPONENT_CAP));
        // Jitter is added, never subtracted (plan §1.3(4)).
        let jitter_max = base_ms / 4;
        let jitter = self.jitter.next_below(jitter_max);
        Duration::from_millis(scaled.saturating_add(jitter))
    }

    /// Consecutive restart count; exposed for `rctl status`.
    pub fn consecutive_restarts(&self) -> u32 {
        self.consecutive
    }

    /// Failures currently inside the sliding window.
    pub fn failures_in_window(&self) -> usize {
        self.window.len()
    }

    /// Whether the breaker is currently tripped.
    pub fn is_tripped(&self) -> bool {
        let width = as_millis(self.interval);
        width != 0 && self.burst != 0 && self.window.len() as u32 >= self.burst
    }

    /// Forget everything; `rctl reset-failed` and a manual start both use this.
    pub fn reset(&mut self) {
        self.consecutive = 0;
        self.window.clear();
        self.became_active_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A deterministic seed, so a failing test is reproducible.
    fn engine(base_secs: u64, burst: u32, interval_secs: u64) -> BackoffState {
        BackoffState::new(
            Duration::from_secs(base_secs),
            burst,
            Duration::from_secs(interval_secs),
            0x1234_5678_9abc_def0,
        )
    }

    /// The plain sequence from plan §1.3(4), with jitter disabled by using a
    /// base whose quarter is zero... which is not expressible, so instead the
    /// bounds are asserted.
    #[test]
    fn the_sequence_doubles_and_then_caps() {
        let mut e = engine(2, 100, 1000);
        let delays: Vec<u64> = (0..9)
            .map(|_| e.on_failure(0).unwrap().as_millis() as u64)
            .collect();

        // base=2s: 2, 4, 8, 16, 32, then capped at 64 (2 * 2^5).
        let bases = [
            2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 64_000, 64_000, 64_000,
        ];
        for (i, (&expected, &got)) in bases.iter().zip(delays.iter()).enumerate() {
            // Jitter is +Random(0, base/4), so the delay is in [base, base+base/4).
            let max = expected + expected / 4;
            assert!(
                (expected..max).contains(&got),
                "attempt {i}: expected {expected}..{max}, got {got}"
            );
        }
    }

    /// Jitter must only ever lengthen a delay - subtracting one could make a
    /// retry fire before the base and re-synchronise services.
    #[test]
    fn jitter_only_ever_adds() {
        for seed in 1..200u64 {
            let mut e = BackoffState::new(
                Duration::from_secs(2),
                1000,
                Duration::from_secs(1000),
                seed,
            );
            let base_ms = 2000u64;
            for attempt in 0..6 {
                let expected = base_ms << attempt.min(BACKOFF_EXPONENT_CAP);
                let got = e.on_failure(0).unwrap().as_millis() as u64;
                assert!(
                    got >= expected,
                    "seed {seed} attempt {attempt}: {got} < {expected}"
                );
                assert!(
                    got < expected + expected / 4 + 1,
                    "seed {seed} attempt {attempt}: jitter too large"
                );
            }
        }
    }

    #[test]
    fn two_units_with_different_seeds_do_not_move_in_lockstep() {
        let mut a = BackoffState::new(Duration::from_secs(2), 100, Duration::from_secs(100), 1);
        let mut b = BackoffState::new(Duration::from_secs(2), 100, Duration::from_secs(100), 2);
        let da: Vec<u64> = (0..6)
            .map(|_| a.on_failure(0).unwrap().as_millis() as u64)
            .collect();
        let db: Vec<u64> = (0..6)
            .map(|_| b.on_failure(0).unwrap().as_millis() as u64)
            .collect();
        assert_ne!(da, db, "identical seeds would restart in lockstep");
    }

    /// The breaker trips once `burst` failures land inside one window.
    #[test]
    fn the_breaker_trips_after_burst_failures() {
        let mut e = engine(1, 3, 10);
        assert!(e.on_failure(1_000).is_some(), "1 of 3");
        assert!(e.on_failure(2_000).is_some(), "2 of 3");
        assert!(e.on_failure(3_000).is_none(), "3 of 3 trips the breaker");
        assert!(e.is_tripped());
    }

    /// Failures that slide out of the window must not count towards the limit.
    #[test]
    fn old_failures_slide_out_of_the_window() {
        let mut e = engine(1, 3, 10); // window 10s, burst 3
        e.on_failure(1_000);
        e.on_failure(2_000);
        // These two are >10s after the first two, so those have slid out.
        e.on_failure(20_000).expect("still room");
        e.on_failure(21_000)
            .expect("the 1s and 2s entries must have expired");
        assert!(!e.is_tripped());
    }

    /// A unit that stays up for a full window resets, so a service that
    /// crashes once a day never trips a breaker.
    #[test]
    fn staying_up_for_a_full_window_resets_the_counter() {
        let mut e = engine(1, 3, 10);
        e.on_failure(0);
        e.on_failure(1_000);
        assert_eq!(e.consecutive_restarts(), 2);

        // Became active, then crashed almost immediately: 100ms < the 10s
        // window, so the sequence keeps growing.
        e.on_active(5_000);
        e.on_failure(5_100);
        assert_eq!(e.consecutive_restarts(), 3, "a short run does not reset");

        // Ran from t=20_000 to t=40_000, which is two full 10s windows.
        e.on_active(20_000);
        e.on_failure(40_000);
        assert_eq!(
            e.consecutive_restarts(),
            1,
            "a stable run resets the sequence"
        );
    }

    /// A window is cleared only when a failure finds the unit had been stable,
    /// since `on_active` alone cannot know the run continued.
    #[test]
    fn the_window_is_cleared_by_a_stable_run() {
        let mut e = engine(1, 3, 10);
        e.on_failure(1_000);
        e.on_failure(2_000);
        assert_eq!(e.failures_in_window(), 2);

        e.on_active(50_000); // over the 10s window
        assert_eq!(
            e.failures_in_window(),
            2,
            "on_active alone must not clear: the unit has not failed yet"
        );

        // The next failure sees a stable run and clears before counting.
        e.on_failure(50_100);
        assert_eq!(
            e.failures_in_window(),
            1,
            "the previous failures were discarded, only this one counts"
        );
    }

    /// Plan §1.1: `StartLimitIntervalSec=0` disables the breaker entirely.
    #[test]
    fn a_zero_interval_disables_the_breaker() {
        let mut e = engine(1, 2, 0);
        for i in 0..50 {
            assert!(e.on_failure(i).is_some(), "attempt {i} must retry");
        }
        assert!(!e.is_tripped());
    }

    /// Plan §1.1: `StartLimitBurst=0` means no limit.
    #[test]
    fn a_zero_burst_means_no_limit() {
        let mut e = engine(1, 0, 10);
        for i in 0..50 {
            assert!(e.on_failure(i * 1000).is_some(), "attempt {i}");
        }
    }

    /// The M4 soak's leak, as a test. With the breaker disabled nothing can read
    /// the window, and a zero interval could never expire anything out of it, so
    /// recording failures there grew it by one `Millis` per restart for as long
    /// as the unit kept restarting - a unit file (`StartLimitIntervalSec=0`)
    /// bought an unbounded allocation.
    #[test]
    fn a_disabled_breaker_records_nothing() {
        let mut e = engine(0, 1000, 0);
        for _ in 0..100_000 {
            assert!(
                e.on_failure(0).is_some(),
                "the breaker is off: always retry"
            );
        }
        assert_eq!(
            e.failures_in_window(),
            0,
            "100,000 failures must not be 100,000 remembered timestamps"
        );
    }

    /// `StartLimitBurst=0` means "no limit", which is the other way to turn the
    /// breaker off, and it must not record either.
    #[test]
    fn a_zero_burst_records_nothing() {
        let mut e = engine(0, 0, 1_000_000);
        for _ in 0..100_000 {
            assert!(e.on_failure(0).is_some());
        }
        assert_eq!(e.failures_in_window(), 0);
    }

    /// The window holds what the breaker needs and no more: `burst` entries,
    /// however long the interval is and however fast the unit restarts.
    #[test]
    fn the_window_never_grows_past_the_burst() {
        // An interval of 11 days, so nothing expires during the test, and a
        // burst of 5: the window may be 5 entries wide, not 10,000.
        let mut e = BackoffState::new(Duration::ZERO, 5, Duration::from_secs(1_000_000), 0x5eed);
        let mut tripped_at = None;
        for millis in 0..10_000 {
            if e.on_failure(millis).is_none() && tripped_at.is_none() {
                tripped_at = Some(millis);
            }
            assert!(
                e.failures_in_window() <= 5,
                "the window grew to {} entries",
                e.failures_in_window()
            );
        }
        assert_eq!(tripped_at, Some(4), "5 failures inside the window trip it");
    }

    /// `rctl reset-failed` and a manual start must both clear the counters.
    #[test]
    fn reset_clears_everything() {
        let mut e = engine(1, 3, 10);
        e.on_failure(1_000);
        e.on_failure(2_000);
        e.on_failure(3_000);
        assert!(e.is_tripped());

        e.reset();
        assert!(!e.is_tripped());
        assert_eq!(e.consecutive_restarts(), 0);
        assert!(
            e.on_failure(4_000).is_some(),
            "retrying must work after a reset"
        );
    }

    /// `RestartSec=0` means restart immediately, with no backoff and no
    /// jitter - plan §1.1 is explicit that zero is meaningful here.
    #[test]
    fn a_zero_base_means_no_delay() {
        let mut e = engine(0, 5, 10);
        assert_eq!(e.on_failure(0), Some(Duration::ZERO));
        assert_eq!(e.on_failure(1), Some(Duration::ZERO));
        assert_eq!(e.on_failure(2), Some(Duration::ZERO));
    }

    /// A huge base must saturate rather than wrapping to a small delay, which
    /// would restart the service far sooner than configured.
    #[test]
    fn a_huge_base_saturates_instead_of_wrapping() {
        let mut e = BackoffState::new(
            Duration::from_secs(u64::MAX / 4),
            1000,
            Duration::from_secs(1000),
            7,
        );
        for _ in 0..8 {
            let d = e.on_failure(0).unwrap();
            assert!(d.as_millis() > 0);
            // The point is only that it did not become small.
            assert!(d.as_secs() > 1_000_000, "unexpectedly small: {d:?}");
        }
    }

    /// The delay grows geometrically while jitter stays within its bound.
    /// Once the base caps, attempts may differ from each other by the jitter
    /// alone - that is the point of it - but never fall below the cap.
    #[test]
    fn the_delay_grows_geometrically_and_respects_the_cap_and_jitter() {
        let mut e = engine(1, 1000, 10_000);
        let base_ms = 1000u64;
        let cap = base_ms * (1 << BACKOFF_EXPONENT_CAP);

        for attempt in 0..10u32 {
            let observed = e.on_failure(0).unwrap().as_millis() as u64;
            let expected_base = base_ms << attempt.min(BACKOFF_EXPONENT_CAP);
            let expected_base = expected_base.min(cap);
            let max_jitter = expected_base / 4;
            assert!(
                (expected_base..=expected_base + max_jitter).contains(&observed),
                "attempt {attempt}: expected {expected_base}..={}, got {observed}",
                expected_base + max_jitter
            );
        }
    }

    #[test]
    fn jitter_of_a_zero_bound_is_zero() {
        let mut j = Jitter::new(99);
        assert_eq!(j.next_below(0), 0);
        // And the bound is respected otherwise.
        for _ in 0..100 {
            assert!(j.next_below(10) < 10);
        }
    }

    #[test]
    fn a_zero_seed_does_not_wedge_the_generator() {
        // A zero state makes xorshift emit zero forever; new() must avoid it.
        let mut j = Jitter::new(0);
        assert_ne!(j.next_below(1000), j.next_below(1000));
    }
}
