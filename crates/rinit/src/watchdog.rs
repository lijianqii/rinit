//! The hardware watchdog as a boot uses it (plan §7.5).
//!
//! [`rinit_runtime::syscall::watchdog`] knows the device protocol; this module
//! decides whether *this* boot has a watchdog at all, how often it is fed, and
//! where the feed comes from.
//!
//! Three decisions live here, and each one is a way to get it wrong:
//!
//! - **`--dev` never arms the default device.** Opening `/dev/watchdog` starts a
//!   hardware timer, and a developer host running a smoke test is not going to
//!   feed it for the next thirty seconds. `--dev` exists to make the unit path
//!   testable on a host; arming the host's watchdog from it would be a bug that
//!   reboots the machine of whoever ran `cargo test`. An *explicitly* named
//!   device is still honoured, because that is the only way the host tests can
//!   drive the protocol - against a file, never the real device.
//! - **No device is not an error** (plan §7.5's degrade case): a board without
//!   watchdog hardware is normal, so the default path stays silent. A device the
//!   operator named is worth one warning line when it cannot be opened, because
//!   they asked for it and would otherwise have to guess.
//! - **The feed comes from the supervisor loop and nowhere else.** That is the
//!   plan's "喂狗必须发生在主循环的健康检查点": a timer thread would keep the
//!   hardware happy while the loop that starts services, reaps children and
//!   answers `rctl` is wedged, which is the opposite of self-healing. The loop
//!   asks [`Watchdog::next_feed_in`] so that a pending feed cannot be slept
//!   through, and calls [`Watchdog::feed_if_due`] once per iteration.
//!
//! What is deliberately *not* covered: the boot itself. The watchdog is armed
//! when the supervisor loop begins, so mounts, unit loading and the first start
//! of every level run outside it. Arming earlier would mean feeding from the boot
//! sequence too, and a level that takes longer than the timeout - a slow service
//! with a long `TimeoutStartSec=` - would reset the machine mid-boot.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rinit_runtime::syscall::watchdog;

use crate::log::rlog;

/// A hardware watchdog this boot feeds, or a plain "there is none".
///
/// The disabled form is the default outcome and the common one: no
/// `--watchdog`, no device, or `--dev`.
pub struct Watchdog {
    /// The open device. `None` is "this boot has no watchdog", and every method
    /// below is then a no-op rather than a special case at the call site.
    dev: Option<watchdog::Watchdog>,
    /// How long between feeds (plan §7.5's `timeout/2`).
    interval: Duration,
    /// When the next feed is due.
    next: Instant,
}

impl Watchdog {
    /// Arm the watchdog for this boot, or decide that it has none.
    ///
    /// `device` is `--watchdog`: `None` means the default path, and `Some` means
    /// the operator named one (which is also how the tests point this at a file
    /// instead of at hardware).
    pub fn prepare(dev_mode: bool, device: Option<PathBuf>, timeout_sec: u64) -> Self {
        let Some(path) = resolve(dev_mode, device.as_deref()) else {
            rlog!("dev mode: not touching the watchdog device");
            return Watchdog::disabled();
        };

        let dev = match watchdog::Watchdog::open(&path) {
            Ok(dev) => dev,
            Err(e) => {
                // Plan §7.5: silently disabled when the device is not there. The
                // silence is for the default path only - a board without
                // watchdog hardware should not fill the console with a complaint
                // about a device nobody asked for.
                if device.is_some() {
                    rlog!("warning: cannot open watchdog {}: {e}", path.display());
                }
                return Watchdog::disabled();
            }
        };

        // Best effort, and the reason `open` and `set_timeout` are separate: a
        // driver without the ioctl still has a working timer, and refusing to
        // arm it would turn a missing feature into a missing watchdog. What
        // matters is feeding the timer that is *really* running, so a refusal
        // falls back to the number the device reports - and to the configured one
        // only when the device will not say either.
        let effective = match dev.set_timeout(timeout_sec) {
            Ok(actual) => actual,
            Err(e) => match dev.timeout() {
                Ok(actual) => {
                    rlog!(
                        "warning: watchdog {}: cannot set the timeout to {timeout_sec}s ({e}); \
                         the device reports {actual}s, which is what the feeds will use",
                        path.display()
                    );
                    actual
                }
                Err(_) => {
                    rlog!(
                        "warning: watchdog {}: cannot set the timeout to {timeout_sec}s ({e}) \
                         and it will not report its own; the feeds will use the configured \
                         {timeout_sec}s",
                        path.display()
                    );
                    timeout_sec
                }
            },
        };

        let interval = feed_interval(effective);
        rlog!(
            "watchdog {} armed: {effective}s timeout, feeding every {interval:?}",
            path.display()
        );
        Watchdog {
            dev: Some(dev),
            interval,
            next: Instant::now(),
        }
    }

    /// A boot with no watchdog.
    fn disabled() -> Self {
        Watchdog {
            dev: None,
            interval: Duration::ZERO,
            next: Instant::now(),
        }
    }

    /// Whether this boot armed a watchdog.
    ///
    /// Used by [`Watchdog::disarm`] to stay silent when there is nothing to
    /// release, and by the tests to say "armed" without reading the device.
    pub const fn is_enabled(&self) -> bool {
        self.dev.is_some()
    }

    /// Feed the device if the interval has elapsed.
    ///
    /// Called once per supervisor-loop iteration (plan §7.5's health
    /// checkpoint), before the loop blocks. The first call feeds immediately:
    /// the device was just armed, and an extra byte costs nothing.
    pub fn feed_if_due(&mut self) {
        if !self.is_enabled() {
            return;
        }
        let now = Instant::now();
        if now < self.next {
            return;
        }
        self.next = now + self.interval;
        if let Some(dev) = &self.dev
            && let Err(e) = dev.feed()
        {
            // Not fatal, and not a reason to stop feeding: a device that
            // answered once may answer again, and giving up would convert a
            // transient error into a reset.
            rlog!("warning: watchdog feed failed: {e}");
        }
    }

    /// How long until the next feed is due, for the loop's `select!` deadline.
    ///
    /// A disabled watchdog must not *shorten* the loop's sleep, so this reports
    /// the same hour the loop already uses when no timer is pending - not
    /// `Duration::MAX`, which would overflow when added to an `Instant`.
    pub fn next_feed_in(&self) -> Duration {
        if !self.is_enabled() {
            return Duration::from_secs(3600);
        }
        self.next.saturating_duration_since(Instant::now())
    }

    /// Disarm at shutdown: the magic close of plan §7.5.
    ///
    /// The opposite case needs no code: a hung rinit never gets here, and that
    /// is what makes the hardware reset the machine. The console line exists so
    /// that a reboot which released the watchdog is distinguishable from one
    /// that raced it - and a boot with no watchdog says nothing, which is the
    /// plan's silent-disable rule.
    pub fn disarm(self) {
        let Some(dev) = self.dev else {
            return;
        };
        match dev.disarm() {
            Ok(()) => rlog!("watchdog disarmed"),
            Err(e) => rlog!("warning: cannot disarm the watchdog: {e}"),
        }
    }
}

/// Which device this boot should open, if any.
///
/// The `--dev` rule is the whole reason this is a separate function: it is the
/// difference between a host test and a rebooted host, and a pure function can
/// be tested without opening anything.
fn resolve(dev_mode: bool, device: Option<&std::path::Path>) -> Option<PathBuf> {
    match device {
        // Named by the operator, so honoured even in `--dev`: the host tests
        // point this at a file, and refusing would make the protocol
        // untestable on a host.
        Some(path) => Some(path.to_path_buf()),
        // The default path in `--dev` is the developer's own hardware. Never.
        None if dev_mode => None,
        None => Some(PathBuf::from(watchdog::DEFAULT_DEVICE)),
    }
}

/// Plan §7.5's feed cadence: half the timeout, so a single missed feed is not
/// already a reset.
///
/// Computed in milliseconds because whole seconds would round a one-second
/// timeout to zero. The floor is belt-and-braces rather than a case anyone can
/// reach by configuration - `--watchdog-timeout` refuses `0`, and
/// [`watchdog::Watchdog::set_timeout`] substitutes the requested value when a
/// driver answers `0` - but a zero here would mean a feed on every iteration of
/// the supervisor loop, which is a spin, not a cadence.
fn feed_interval(timeout_sec: u64) -> Duration {
    Duration::from_millis(timeout_sec.saturating_mul(1000) / 2).max(Duration::from_millis(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A stand-in device: an ordinary file, which is what the explicit
    /// `--watchdog` path makes testable.
    struct Device(PathBuf);

    impl Device {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("rinit-wd-policy-{}-{tag}", std::process::id()));
            std::fs::write(&path, b"").expect("create stand-in device");
            Device(path)
        }

        fn bytes(&self) -> Vec<u8> {
            std::fs::read(&self.0).unwrap_or_default()
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// The rule that keeps a developer host off the hardware timer.
    #[test]
    fn dev_mode_does_not_resolve_the_default_device() {
        assert_eq!(resolve(true, None), None);
    }

    /// ...while a real boot still does, and a named device wins in both.
    #[test]
    fn the_default_device_is_used_for_a_real_boot_and_a_named_one_wins() {
        assert_eq!(
            resolve(false, None),
            Some(PathBuf::from(watchdog::DEFAULT_DEVICE))
        );
        let named = Path::new("/tmp/not-the-real-one");
        assert_eq!(resolve(false, Some(named)), Some(named.to_path_buf()));
        assert_eq!(resolve(true, Some(named)), Some(named.to_path_buf()));
    }

    /// Plan §7.5's cadence, and the reason it is millisecond arithmetic.
    #[test]
    fn the_feed_interval_is_half_the_timeout_and_never_zero() {
        assert_eq!(feed_interval(30), Duration::from_secs(15));
        assert_eq!(feed_interval(120), Duration::from_secs(60));
        assert_eq!(feed_interval(3), Duration::from_millis(1500));
        assert_eq!(feed_interval(1), Duration::from_millis(500));
        assert_eq!(feed_interval(0), Duration::from_millis(1));
    }

    /// An armed watchdog feeds on the first iteration, and the interval is what
    /// stops it from feeding on every iteration: a supervisor loop that spins
    /// must not turn into a byte-per-iteration firehose.
    #[test]
    fn an_armed_watchdog_feeds_once_per_interval_and_disarms_on_shutdown() {
        let dev = Device::new("armed");
        let mut watchdog = Watchdog::prepare(false, Some(dev.0.clone()), 4);
        assert!(
            watchdog.is_enabled(),
            "an openable device arms the watchdog"
        );

        watchdog.feed_if_due();
        watchdog.feed_if_due();
        watchdog.feed_if_due();
        assert_eq!(dev.bytes(), b"0", "only the first call is due");

        // The interval was shortened to half of the four seconds, so the next
        // feed is due within that window but not after the whole timeout.
        assert!(watchdog.next_feed_in() <= Duration::from_secs(2));
        assert!(watchdog.next_feed_in() > Duration::from_millis(1));

        watchdog.disarm();
        assert_eq!(
            dev.bytes(),
            b"0V",
            "the shutdown path writes the magic close"
        );
    }

    /// Plan §7.5's silent disable, in its two flavours: `--dev` with no device
    /// named, and a named device that is not there.
    #[test]
    fn a_watchdog_that_is_not_there_is_disabled_and_harmless() {
        let missing = std::env::temp_dir().join("rinit-wd-policy-does-not-exist");
        let mut explicit = Watchdog::prepare(false, Some(missing), 30);
        assert!(!explicit.is_enabled());
        // The loop's deadline is untouched, and every call is a no-op.
        assert_eq!(explicit.next_feed_in(), Duration::from_secs(3600));
        explicit.feed_if_due();
        explicit.disarm();

        let mut dev = Watchdog::prepare(true, None, 30);
        assert!(!dev.is_enabled());
        assert_eq!(dev.next_feed_in(), Duration::from_secs(3600));
        dev.feed_if_due();
        dev.disarm();
    }

    /// A device that is there but is not a watchdog: the ioctl is refused, the
    /// device stays armed, and the feeds use the timeout it reports - falling
    /// back to the configured one when it will not even say that.
    #[test]
    fn a_device_that_refuses_the_timeout_ioctl_is_still_fed() {
        let dev = Device::new("no-ioctl");
        let mut watchdog = Watchdog::prepare(false, Some(dev.0.clone()), 30);
        assert!(
            watchdog.is_enabled(),
            "the open succeeded, so there is a watchdog to feed"
        );
        watchdog.feed_if_due();
        assert_eq!(dev.bytes(), b"0");
        assert!(
            watchdog.next_feed_in() <= Duration::from_secs(15),
            "a refused timeout falls back to the configured one, so the \
             interval is still half of it"
        );
    }
}
