//! The hardware watchdog device (plan §7.5).
//!
//! Three things happen here, and each is one syscall: `open(2)` the device
//! (which is what *arms* it - there is no separate "start"), `write(2)` a byte
//! to feed it, and `write(2)` of the magic character to tell the driver that the
//! close is deliberate. `ioctl(2)` is attempted to put the configured timeout in
//! force, but it is best effort: a driver that does not implement it still has a
//! working timer, and the caller reports what the device said rather than
//! assuming the request took.
//!
//! The device path is a *parameter*, like the cgroup directory in
//! [`crate::syscall::cgroup`]: pointed at an ordinary file, the whole protocol -
//! feed bytes, magic close, disarm - is checkable on a developer host that must
//! not go anywhere near the real `/dev/watchdog`.
//!
//! Whether a watchdog should be armed at all is policy and lives in the binary
//! (`rinit::watchdog`); this module only knows how.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::error::SyscallError;
use crate::syscall::ioctl_request;

/// Where the watchdog device normally lives (plan §7.5's `WatchdogDevice=`).
pub const DEFAULT_DEVICE: &str = "/dev/watchdog";

/// Default hardware timeout in seconds (plan §7.5's `WatchdogTimeoutSec=`).
///
/// The device's own default is whatever the driver chose, which is why the
/// timeout is set rather than assumed.
pub const DEFAULT_TIMEOUT_SEC: u64 = 30;

/// The byte written to feed the watchdog. The plan writes `"0"`, and any byte
/// would do; a byte that is not `V` is the point.
const FEED: u8 = b'0';

/// The magic character: written before a close, it tells the driver the close is
/// deliberate ("magic close", plan §7.5) instead of the last breath of whoever
/// was feeding it.
const MAGIC_CLOSE: u8 = b'V';

/// `WDIOC_SETTIMEOUT`, i.e. `_IOWR(WATCHDOG_IOCTL_BASE, 6, int)` from
/// `include/uapi/linux/watchdog.h`, where `WATCHDOG_IOCTL_BASE` is `'W'`.
///
/// Spelled out because `libc` does not carry it (checked: no `WDIOC` in libc
/// 0.2.190). The encoding is `asm-generic/ioctl.h`'s, and the test at the bottom
/// of this module recomputes both numbers from that layout - `_IOC(dir, type,
/// nr, size)` - rather than trusting the literals. The literals themselves were
/// checked against the C header with a host and an aarch64 compiler, so they are
/// not arch-specific (the `_IOC` layout is the shared one on both).
pub const WDIOC_SETTIMEOUT: libc::c_ulong = 0xc004_5706;

/// `WDIOC_GETTIMEOUT`, i.e. `_IOR(WATCHDOG_IOCTL_BASE, 7, int)`.
pub const WDIOC_GETTIMEOUT: libc::c_ulong = 0x8004_5707;

/// An open watchdog device.
///
/// Holding one means the hardware timer is running: the device is armed by the
/// `open(2)` that produced it, and it stays armed until the driver is told the
/// close is deliberate. That is why there is no way to construct this type
/// without opening a device, and why every way out of scope disarms it - see
/// [`Watchdog::disarm`] and the `Drop` impl below.
#[derive(Debug)]
pub struct Watchdog {
    /// The device. Rust opens with `O_CLOEXEC`, which is load-bearing twice
    /// over here: a service must not inherit a handle that could disarm the
    /// machine's watchdog, and an inherited handle would keep the device open
    /// after rinit is gone, feeding or not feeding it on someone else's behalf.
    device_file: File,
    /// Kept for logging and for `Disarm` decisions; the path is not used again.
    path: PathBuf,
    /// Cleared once the magic close has been written, so the `Drop` impl cannot
    /// write a second one.
    armed: bool,
}

impl Watchdog {
    /// Open `device`, which arms it.
    ///
    /// No timeout is set here; call [`Watchdog::set_timeout`] for that. The two
    /// are separate because the caller's policy differs: not being able to open
    /// the device means there is no watchdog (plan §7.5's silent-disable case),
    /// while not being able to set the timeout means there is one whose timer is
    /// whatever the driver picked.
    ///
    /// # Errors
    /// [`SyscallError`] from the `open(2)` - `ENOENT` for a board with no
    /// watchdog hardware, `EBUSY` when another process (another init, or a
    /// `watchdogd`) already holds it, `EACCES` without permission.
    pub fn open(device: &Path) -> Result<Self, SyscallError> {
        let device_file = std::fs::OpenOptions::new()
            .write(true)
            .open(device)
            .map_err(|e| SyscallError::from_io("open(watchdog)", &e))?;
        Ok(Watchdog {
            device_file,
            path: device.to_path_buf(),
            armed: true,
        })
    }

    /// The device this watchdog came from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Ask the driver to use `timeout_sec`, returning the timeout actually in
    /// force.
    ///
    /// `WDIOC_SETTIMEOUT` is a read-write ioctl: on success the driver writes
    /// back what it settled on, which is not always the request (many drivers
    /// clamp to their own maximum, and the watchdog core rounds to the device's
    /// granularity). That returned value is what the caller has to feed against.
    ///
    /// # Errors
    /// [`SyscallError`] when the driver does not implement the ioctl (`ENOTTY`
    /// on a device or a stand-in that has no watchdog behind it), or refuses the
    /// value (`EINVAL`).
    pub fn set_timeout(&self, timeout_sec: u64) -> Result<u64, SyscallError> {
        let mut seconds: libc::c_int = timeout_sec.min(libc::c_int::MAX as u64) as libc::c_int;
        // SAFETY: `WDIOC_SETTIMEOUT` takes a pointer to an `int`, and `seconds`
        // is one for the duration of the call. Nothing else touches it.
        let rc = unsafe {
            libc::ioctl(
                self.device_file.as_raw_fd(),
                ioctl_request(WDIOC_SETTIMEOUT),
                &mut seconds,
            )
        };
        if rc < 0 {
            return Err(SyscallError::last_os_error("ioctl(WDIOC_SETTIMEOUT)"));
        }
        // The kernel may answer with 0 for "unchanged"; 0 is not a timeout.
        Ok(if seconds > 0 {
            seconds as u64
        } else {
            timeout_sec
        })
    }

    /// What the driver says its timeout is, in seconds.
    ///
    /// Used only to report honestly when [`Watchdog::set_timeout`] was refused:
    /// the feed interval has to be short enough for the timer that is really
    /// running, not for the one that was asked for.
    ///
    /// # Errors
    /// [`SyscallError`] when the driver does not implement `WDIOC_GETTIMEOUT`.
    pub fn timeout(&self) -> Result<u64, SyscallError> {
        let mut seconds: libc::c_int = 0;
        // SAFETY: `WDIOC_GETTIMEOUT` takes a pointer to an `int`, and `seconds`
        // is one for the duration of the call. Nothing else touches it.
        let rc = unsafe {
            libc::ioctl(
                self.device_file.as_raw_fd(),
                ioctl_request(WDIOC_GETTIMEOUT),
                &mut seconds,
            )
        };
        if rc < 0 {
            return Err(SyscallError::last_os_error("ioctl(WDIOC_GETTIMEOUT)"));
        }
        if seconds > 0 {
            return Ok(seconds as u64);
        }
        Err(SyscallError::new("ioctl(WDIOC_GETTIMEOUT)", libc::EINVAL))
    }

    /// Feed the watchdog.
    ///
    /// # Errors
    /// [`SyscallError`] from the `write(2)` - `ENODEV` once the device has been
    /// closed out from under us, `EIO` on a driver that has given up.
    pub fn feed(&self) -> Result<(), SyscallError> {
        self.write_byte(FEED, "write(watchdog)")
    }

    /// Disarm and close: write the magic close, then let the descriptor go.
    ///
    /// This is the graceful-shutdown path (plan §7.5). A watchdog that is
    /// *hung* never reaches it, which is the whole mechanism: no magic close,
    /// so the hardware resets the machine.
    pub fn disarm(mut self) -> Result<(), SyscallError> {
        self.magic_close()
    }

    /// Write the magic close if it has not been written already, and remember
    /// that it has.
    fn magic_close(&mut self) -> Result<(), SyscallError> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        self.write_byte(MAGIC_CLOSE, "write(watchdog V)")
    }

    /// One byte to the device.
    ///
    /// No `flush` follows, unlike the cgroup attribute writes: those go through
    /// a `File` the caller keeps and reuses, and the explicit flush is there to
    /// say that the value is not sitting in a buffer. `File` has no user-space
    /// buffer at all, so `write_all` returning means the driver has the byte -
    /// and for a watchdog that is the difference between fed and not fed.
    fn write_byte(&self, byte: u8, op: &'static str) -> Result<(), SyscallError> {
        // `write_all` is what retries on `EINTR`; a signal arriving between two
        // feeds must not become a skipped feed.
        (&self.device_file)
            .write_all(&[byte])
            .map_err(|e| SyscallError::from_io(op, &e))
    }
}

/// Leaving scope writes the magic close.
///
/// Not a tidiness measure: `open(2)` starts a hardware timer, and a driver built
/// with `nowayout` keeps it running when the descriptor is closed without the
/// magic character. Every early return, every error path after a successful
/// open, and every scope exit therefore has to disarm - including the ones the
/// author did not think of. The graceful path calls [`Watchdog::disarm`]
/// explicitly, which clears the flag so this cannot write a second `V`.
///
/// The intended failure mode is a *hang*, and a hung process does not run
/// `Drop`, so this does not weaken the reset-on-hang mechanism. In the release
/// profile a panic aborts before unwinding, so it does not disarm either.
impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.magic_close();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// A throwaway "device": an ordinary file, which is what makes this module
    /// testable without a watchdog and without risking the host's own.
    struct Device(PathBuf);

    impl Device {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("rinit-watchdog-{}-{tag}", std::process::id()));
            std::fs::write(&path, b"").expect("create stand-in device");
            Device(path)
        }

        fn path(&self) -> &Path {
            &self.0
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

    /// The protocol is a byte per feed and a different byte to disarm; a
    /// stand-in file is where that is observable.
    #[test]
    fn feeding_writes_the_feed_byte_and_disarming_writes_the_magic_close() {
        let dev = Device::new("protocol");
        let watchdog = Watchdog::open(dev.path()).expect("open stand-in device");
        assert_eq!(watchdog.path(), dev.path());

        watchdog.feed().expect("feed");
        watchdog.feed().expect("feed");
        assert_eq!(dev.bytes(), b"00", "two feeds are two bytes");

        watchdog.disarm().expect("disarm");
        assert_eq!(
            dev.bytes(),
            b"00V",
            "the magic close is written after the feeds, and only once"
        );
    }

    /// The safety property, and the reason `Drop` is implemented at all: a
    /// watchdog that leaves scope without an explicit `disarm` must still tell
    /// the driver the close is deliberate.
    #[test]
    fn dropping_an_armed_watchdog_disarms_it() {
        let dev = Device::new("drop");
        {
            let watchdog = Watchdog::open(dev.path()).expect("open stand-in device");
            watchdog.feed().expect("feed");
        }
        assert_eq!(
            dev.bytes(),
            b"0V",
            "going out of scope writes the magic close"
        );
    }

    /// And the explicit path does not end up writing it twice: one `V` is a
    /// disarm, two are a fed byte after one.
    #[test]
    fn an_explicit_disarm_is_not_repeated_by_drop() {
        let dev = Device::new("once");
        let watchdog = Watchdog::open(dev.path()).expect("open stand-in device");
        watchdog.disarm().expect("disarm");
        assert_eq!(dev.bytes(), b"V");
    }

    /// Plan §7.5's degrade case starts here: no device is not an error to
    /// report, it is the answer "there is no watchdog".
    #[test]
    fn a_missing_device_is_enoent() {
        let missing = std::env::temp_dir().join("rinit-watchdog-does-not-exist");
        let err = Watchdog::open(&missing).expect_err("a missing device cannot be opened");
        assert_eq!(err.errno(), libc::ENOENT);
        assert_eq!(err.op(), "open(watchdog)");
    }

    /// A stand-in device has no watchdog behind it, so both ioctls fail - and
    /// `ENOTTY` is exactly what a driver without `WDIOC_*` support answers. The
    /// caller has to survive that with the device still armed, which is why
    /// these are separate from `open`.
    #[test]
    fn a_device_without_the_ioctls_reports_them_as_unsupported() {
        let dev = Device::new("ioctl");
        let watchdog = Watchdog::open(dev.path()).expect("open stand-in device");

        let err = watchdog
            .set_timeout(30)
            .expect_err("an ordinary file has no WDIOC_SETTIMEOUT");
        assert_eq!(err.op(), "ioctl(WDIOC_SETTIMEOUT)");
        assert_eq!(err.errno(), libc::ENOTTY);

        let err = watchdog
            .timeout()
            .expect_err("an ordinary file has no WDIOC_GETTIMEOUT");
        assert_eq!(err.errno(), libc::ENOTTY);
    }

    /// The two ioctl numbers are the only magic in this module, so they are
    /// recomputed from `asm-generic/ioctl.h`'s layout - `_IOC(dir, type, nr,
    /// size)` - instead of being trusted. `dir` is 2 for read and 1 for write,
    /// the size is `sizeof(int)`, and the type is `'W'`.
    #[test]
    fn the_ioctl_numbers_are_the_ones_the_kernel_header_defines() {
        const fn ioc(dir: u64, kind: u64, nr: u64, size: u64) -> libc::c_ulong {
            ((dir << 30) | (size << 16) | (kind << 8) | nr) as libc::c_ulong
        }
        const READ: u64 = 2;
        const WRITE: u64 = 1;
        let base = b'W' as u64;
        assert_eq!(WDIOC_SETTIMEOUT, ioc(READ | WRITE, base, 6, 4));
        assert_eq!(WDIOC_GETTIMEOUT, ioc(READ, base, 7, 4));
    }
}
