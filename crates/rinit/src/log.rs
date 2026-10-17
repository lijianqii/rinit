//! Minimal, dependency-free logging for early boot.
//!
//! Writes to stdout (fd 1), which the kernel wires to `/dev/console` for PID 1
//! (plan §2.1⑦). The macro is self-contained: it brings `io::Write` into scope
//! internally so callers do not need any import.
//!
//! This is the one log sink rinit does not bound by construction - the ring is
//! per unit and byte-capped, the file mirror truncates - so it is rate limited
//! per call site (see `rinit_core::ratelimit` for the policy and for why the key
//! is the call site rather than the message text). A service that crash-loops
//! restarts hundreds of times a second and prints the same handful of messages
//! with different pids in them, which is exactly what a text-keyed limiter would
//! fail to fold: 955 MB of console in five hours was the soak's version of this.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use rinit_core::ratelimit::{Decision, Key, Limiter};

/// A call site may print `RATE_BURST` messages every `RATE_WINDOW_MS`.
///
/// 40 in 5 s is 8/s per call site: low enough to fold a crash loop (thousands per
/// second), high enough that no legitimate burst is touched. The largest real one
/// is a boot level starting its units, where 20 services print `started X as pid
/// N` through a single call site in the same instant - and the guest asserts on
/// all 20 of those lines.
const RATE_WINDOW_MS: u64 = 5_000;
const RATE_BURST: u32 = 40;

static LIMITER: Mutex<Limiter> = Mutex::new(Limiter::new(RATE_WINDOW_MS, RATE_BURST));
static START: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the first log line, from a monotonic clock.
///
/// `Instant` rather than the wall clock: a rate limiter that a clock step can
/// confuse is a rate limiter that either folds everything or nothing.
fn now_ms() -> u64 {
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

/// Whether this call site may print right now.
///
/// Reports the previous window's suppressed count itself, immediately before the
/// message that resumes - that line is not rate limited, because it is the one
/// that explains the gap.
pub(crate) fn admit(file: &'static str, line: u32) -> bool {
    let decision = {
        // A poisoned lock would mean another thread panicked while holding it;
        // there is nothing to do about that here, and dropping log lines because
        // of it would hide the thing that matters.
        let mut limiter = LIMITER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        limiter.admit(Key { file, line }, now_ms())
    };

    match decision {
        Decision::Print => true,
        Decision::Suppress => false,
        Decision::PrintAfterSuppressing(count) => {
            emit(&format!(
                "warning: {count} message(s) suppressed at {file}:{line} (rate limit)"
            ));
            true
        }
    }
}

/// Report what the limiter is still holding.
///
/// A flood that stops leaves its count in the table, and the log would otherwise
/// end mid-sentence. Called once, on the shutdown path.
pub(crate) fn flush_suppressed() {
    let pending = {
        let limiter = LIMITER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        limiter.pending()
    };
    for (key, count) in pending {
        emit(&format!(
            "warning: {count} message(s) suppressed at {}:{} (rate limit)",
            key.file, key.line
        ));
    }
}

/// Write one already-formatted line to the console.
pub(crate) fn emit(line: &str) {
    let mut out = std::io::stdout();
    let _ = std::io::Write::write_fmt(&mut out, format_args!("rinit: {line}\n"));
    let _ = std::io::Write::flush(&mut out);
}

macro_rules! rlog {
    ($($arg:tt)*) => {{
        if $crate::log::admit(file!(), line!()) {
            let line = format!($($arg)*);
            $crate::log::emit(&line);
        }
    }};
}

pub(crate) use rlog;

#[cfg(test)]
mod tests {
    use super::*;

    /// The macro has to keep the message out of the limiter's way when it is
    /// under the burst: one call, one line, and no suppression notice.
    #[test]
    fn an_ordinary_message_is_printed_once() {
        // The limiter is process-global, so this test drives its own keys rather
        // than the macro's real ones (another test's flood must not affect it).
        let mut limiter = Limiter::new(RATE_WINDOW_MS, RATE_BURST);
        assert_eq!(
            limiter.admit(
                Key {
                    file: "log.rs",
                    line: 1
                },
                0
            ),
            Decision::Print
        );
        assert!(limiter.pending().is_empty());
    }

    /// The clock is monotonic and starts near zero.
    #[test]
    fn the_clock_is_monotonic_and_bounded() {
        let first = now_ms();
        let second = now_ms();
        assert!(second >= first);
        assert!(second < 60_000, "a test cannot have taken a minute");
    }
}
