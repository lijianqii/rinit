//! Deadline arithmetic that cannot abort PID 1 (plan §3 M4).
//!
//! Every timeout rinit uses comes from a unit file (`TimeoutStartSec=`,
//! `TimeoutStopSec=`, `RestartSec=`, `HealthIntervalSec=`), and the parser only
//! bounds those at `u64::MAX`. `Instant + Duration` *panics* when the sum
//! leaves the representable range, and rinit builds release with
//! `overflow-checks = true` (see the workspace `Cargo.toml`), so a unit file
//! naming an absurd timeout would abort init - exactly the failure mode M4
//! exists to close.
//!
//! Clamping first turns "an absurd timeout" into "longer than the machine will
//! ever be up", which is what the author of such a timeout meant. The clamp
//! lives here, at the point of arithmetic, rather than in the parser, so that
//! the parsed value still reports what the file said.

use std::time::{Duration, Instant};

/// The longest deadline rinit will compute: `2^40` seconds, about 34,000 years.
///
/// This is not a policy limit - it is a value chosen to stay inside `Instant`'s
/// range on every platform rinit targets, so the addition below cannot panic.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(1 << 40);

/// `timeout`, capped at [`MAX_TIMEOUT`].
pub fn clamp(timeout: Duration) -> Duration {
    timeout.min(MAX_TIMEOUT)
}

/// `now + timeout`, clamped so the addition cannot overflow.
pub fn after(timeout: Duration) -> Instant {
    let now = Instant::now();
    // The clamp keeps this in range; `checked_add` states that the arithmetic
    // is checked rather than assumed. The fallback can only be reached on a
    // platform where even `MAX_TIMEOUT` is too large, and "give up now" is the
    // safe reading there - it is still not a panic in PID 1.
    now.checked_add(clamp(timeout)).unwrap_or(now)
}

/// A deadline `secs` in the future, clamped like [`after`].
pub fn after_secs(secs: u64) -> Instant {
    after(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this module exists for: `TimeoutStartSec=18446744073709551615`
    /// in a unit file used to overflow `Instant + Duration` and abort init.
    #[test]
    fn an_absurd_timeout_clamps_instead_of_panicking() {
        let now = Instant::now();
        let deadline = after_secs(u64::MAX);

        assert!(
            deadline > now,
            "an absurd timeout must still be in the future"
        );
        assert!(
            deadline > now + Duration::from_secs(365 * 24 * 60 * 60),
            "it must clamp to something far away, not to now"
        );
    }

    #[test]
    fn ordinary_timeouts_are_left_alone() {
        let now = Instant::now();
        let deadline = after(Duration::from_secs(30));

        assert!(deadline > now);
        assert!(deadline <= now + Duration::from_secs(31));
    }

    #[test]
    fn clamp_caps_only_what_is_too_large() {
        assert_eq!(clamp(Duration::ZERO), Duration::ZERO);
        assert_eq!(clamp(Duration::from_secs(5)), Duration::from_secs(5));
        assert_eq!(clamp(MAX_TIMEOUT), MAX_TIMEOUT);
        assert_eq!(clamp(Duration::from_secs(u64::MAX)), MAX_TIMEOUT);
    }
}
