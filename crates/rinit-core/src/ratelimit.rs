//! Rate limiting for rinit's own console output.
//!
//! The console is the one log sink rinit does not bound. The per-unit ring is
//! byte-capped, the file mirror truncates, and both are about *service* output -
//! but a line rinit prints itself goes to fd 1 and is gone. A service that
//! crash-loops therefore writes at whatever speed the supervisor can restart it,
//! and the 72-hour soak found the consequence the hard way: 955 MB of console in
//! five hours, which filled the tmpfs the run was using and killed the soak with
//! ENOSPC rather than with anything it was measuring.
//!
//! The policy is **per call site**, because that is what keeps a flood from
//! hiding everything else. A unit that restarts five hundred times a second
//! produces the same handful of distinct messages (`started X as pid N`, `reaped
//! pid N`, `X will restart in Y`), and a limiter keyed by the message *text*
//! would not catch them at all - the pids differ every time. Keyed by call site,
//! the flood is folded and an unrelated warning still gets through.
//!
//! What is suppressed is counted and reported the next time that call site is
//! allowed to print, so the count is never silently lost; a flood that stops
//! entirely is reported by [`Limiter::pending`], which the shutdown path flushes.
//!
//! The clock is a parameter rather than `Instant`, which keeps this a pure
//! decision that can be tested without waiting (plan §0.2: the decision belongs
//! in `rinit-core`, the side effect in the binary).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// One call site: the file it is in and the line it is on.
///
/// Both are `'static` because they come from `file!()`/`line!()`, so the table
/// below is keyed by things that never allocate and never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    /// `file!()` of the call site.
    pub file: &'static str,
    /// `line!()` of the call site.
    pub line: u32,
}

/// What a call site is allowed to do with the message it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Print it.
    Print,
    /// Drop it; it has been counted.
    Suppress,
    /// Print it, after saying that `u32` earlier messages from this call site
    /// were suppressed.
    PrintAfterSuppressing(u32),
}

/// One call site's window.
#[derive(Debug, Clone, Copy)]
struct Site {
    /// When the current window opened.
    window_start_ms: u64,
    /// How many messages have been printed in it.
    printed: u32,
    /// How many have been dropped in it.
    suppressed: u32,
}

/// A per-call-site token bucket over a fixed window.
#[derive(Debug)]
pub struct Limiter {
    window_ms: u64,
    burst: u32,
    sites: BTreeMap<Key, Site>,
}

impl Limiter {
    /// A limiter that lets `burst` messages per call site through every
    /// `window_ms`.
    ///
    /// `const` so the binary can hold one in a `static`, which is why there is no
    /// constructor that allocates.
    #[must_use]
    pub const fn new(window_ms: u64, burst: u32) -> Self {
        Limiter {
            window_ms,
            burst,
            sites: BTreeMap::new(),
        }
    }

    /// The number of call sites seen so far.
    ///
    /// The table is bounded by the number of `rlog!` call sites in the binary
    /// (the key is `file!()`/`line!()`, not a unit name or a pid), so it never
    /// needs pruning - this is here so a test can say so.
    #[must_use]
    pub fn sites(&self) -> usize {
        self.sites.len()
    }

    /// Decide what to do with a message from `key` at `now_ms`.
    ///
    /// `now_ms` is a monotonic millisecond count; only differences matter.
    pub fn admit(&mut self, key: Key, now_ms: u64) -> Decision {
        let window_ms = self.window_ms;
        let burst = self.burst;
        let site = self.sites.entry(key).or_insert(Site {
            window_start_ms: now_ms,
            printed: 0,
            suppressed: 0,
        });

        // A window that has rolled over starts fresh, and whatever it dropped is
        // reported now - this is the message the operator sees instead of the
        // thousands that were folded into it.
        if now_ms.saturating_sub(site.window_start_ms) >= window_ms {
            let suppressed = core::mem::take(&mut site.suppressed);
            site.window_start_ms = now_ms;
            site.printed = 1;
            return if suppressed > 0 {
                Decision::PrintAfterSuppressing(suppressed)
            } else {
                Decision::Print
            };
        }

        // Within a window the budget only decreases, so a site that is still
        // under its burst has suppressed nothing yet: the count is reported when
        // the window rolls, below.
        if site.printed < burst {
            site.printed += 1;
            return Decision::Print;
        }

        site.suppressed = site.suppressed.saturating_add(1);
        Decision::Suppress
    }

    /// Every call site that has suppressed something and not reported it yet.
    ///
    /// A flood that stops leaves its count here; the shutdown path prints it so
    /// the log ends with an accurate total rather than an unexplained gap.
    #[must_use]
    pub fn pending(&self) -> Vec<(Key, u32)> {
        self.sites
            .iter()
            .filter(|(_, site)| site.suppressed > 0)
            .map(|(key, site)| (*key, site.suppressed))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(line: u32) -> Key {
        Key {
            file: "test.rs",
            line,
        }
    }

    /// Under the burst, everything prints - a limiter that folded ordinary
    /// messages would be worse than none.
    #[test]
    fn messages_under_the_burst_are_printed() {
        let mut limiter = Limiter::new(5_000, 3);
        for i in 0..3 {
            assert_eq!(limiter.admit(key(1), 1_000 + i), Decision::Print, "{i}");
        }
    }

    /// Over it, the rest are dropped - and counted.
    #[test]
    fn messages_over_the_burst_are_suppressed() {
        let mut limiter = Limiter::new(5_000, 3);
        for _ in 0..3 {
            assert_eq!(limiter.admit(key(1), 1_000), Decision::Print);
        }
        assert_eq!(limiter.admit(key(1), 1_001), Decision::Suppress);
        assert_eq!(limiter.admit(key(1), 1_002), Decision::Suppress);
        assert_eq!(limiter.pending(), alloc::vec![(key(1), 2)]);
    }

    /// The window rolling over is where the count comes back out: the first
    /// message of the next window carries it.
    #[test]
    fn a_new_window_reports_what_the_last_one_suppressed() {
        let mut limiter = Limiter::new(5_000, 2);
        assert_eq!(limiter.admit(key(1), 0), Decision::Print);
        assert_eq!(limiter.admit(key(1), 1), Decision::Print);
        assert_eq!(limiter.admit(key(1), 2), Decision::Suppress);
        assert_eq!(limiter.admit(key(1), 3), Decision::Suppress);
        assert_eq!(limiter.admit(key(1), 4), Decision::Suppress);

        assert_eq!(
            limiter.admit(key(1), 5_000),
            Decision::PrintAfterSuppressing(3),
            "the first message after the window reports the three that were folded"
        );
        assert_eq!(
            limiter.admit(key(1), 5_001),
            Decision::Print,
            "and the count is reported once, not on every message"
        );
    }

    /// Counts belong to a window rather than accumulating: a site suppressed in
    /// two consecutive windows reports each window's total, not a running sum.
    #[test]
    fn each_window_reports_its_own_count() {
        let mut limiter = Limiter::new(1_000, 1);
        assert_eq!(limiter.admit(key(1), 0), Decision::Print);
        for _ in 0..4 {
            assert_eq!(limiter.admit(key(1), 1), Decision::Suppress);
        }
        assert_eq!(
            limiter.admit(key(1), 1_000),
            Decision::PrintAfterSuppressing(4)
        );

        for _ in 0..2 {
            assert_eq!(limiter.admit(key(1), 1_001), Decision::Suppress);
        }
        assert_eq!(
            limiter.admit(key(1), 2_000),
            Decision::PrintAfterSuppressing(2),
            "the second window's two, not the six in total"
        );
    }

    /// The point of keying by call site: one site's flood does not silence
    /// another's message.
    #[test]
    fn a_flood_at_one_call_site_does_not_suppress_another() {
        let mut limiter = Limiter::new(5_000, 1);
        assert_eq!(limiter.admit(key(1), 0), Decision::Print);
        assert_eq!(limiter.admit(key(1), 1), Decision::Suppress);
        assert_eq!(limiter.admit(key(1), 2), Decision::Suppress);

        assert_eq!(
            limiter.admit(key(2), 3),
            Decision::Print,
            "a different call site has its own budget"
        );
    }

    /// Windows are per call site, so a quiet site is not penalised for another's
    /// activity, and the same site starts counting again after its window.
    #[test]
    fn a_window_is_per_call_site() {
        let mut limiter = Limiter::new(1_000, 1);
        assert_eq!(limiter.admit(key(1), 0), Decision::Print);
        assert_eq!(limiter.admit(key(1), 500), Decision::Suppress);
        assert_eq!(
            limiter.admit(key(1), 1_000),
            Decision::PrintAfterSuppressing(1)
        );
        assert_eq!(limiter.admit(key(2), 1_000), Decision::Print);
    }

    /// A flood that stops leaves its count pending, which is what the shutdown
    /// flush reads - otherwise the log would just stop mid-sentence.
    #[test]
    fn pending_reports_a_flood_that_stopped() {
        let mut limiter = Limiter::new(5_000, 1);
        assert_eq!(limiter.admit(key(7), 0), Decision::Print);
        for _ in 0..10 {
            assert_eq!(limiter.admit(key(7), 1), Decision::Suppress);
        }

        assert_eq!(limiter.pending(), alloc::vec![(key(7), 10)]);
        assert_eq!(limiter.sites(), 1, "the table is one entry per call site");
    }

    /// Saturating, not wrapping: a window that suppressed `u32::MAX` messages is
    /// a broken clock rather than a reason to abort init.
    #[test]
    fn the_suppressed_count_saturates() {
        let mut limiter = Limiter::new(5_000, 0);
        assert_eq!(limiter.admit(key(1), 0), Decision::Suppress);
        assert_eq!(limiter.pending(), alloc::vec![(key(1), 1)]);

        let mut limiter = Limiter::new(5_000, 0);
        limiter.admit(key(1), 0);
        // Not reachable in a test in finite time, but the arithmetic is what is
        // under test here: `saturating_add`, not `+`.
        let site = limiter.sites.get_mut(&key(1)).expect("just inserted");
        site.suppressed = u32::MAX;
        assert_eq!(limiter.admit(key(1), 1), Decision::Suppress);
        assert_eq!(limiter.pending(), alloc::vec![(key(1), u32::MAX)]);
    }

    /// A clock that goes backwards (or the first call) must not panic or open a
    /// new window every time.
    #[test]
    fn a_non_monotonic_clock_is_tolerated() {
        let mut limiter = Limiter::new(1_000, 1);
        assert_eq!(limiter.admit(key(1), 10_000), Decision::Print);
        assert_eq!(
            limiter.admit(key(1), 9_000),
            Decision::Suppress,
            "an earlier timestamp is inside the same window"
        );
    }
}
