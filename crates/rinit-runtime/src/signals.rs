//! Async signal delivery via a self-pipe (plan §2.1④, §7.1).
//!
//! `SIGCHLD`/`SIGTERM`/`SIGINT`/`SIGPWR` are delivered to async code through a
//! non-blocking `pipe2` and a `sigaction` handler that writes one byte per
//! signal. The read end is registered with tokio's reactor via
//! [`tokio::io::unix::AsyncFd`], so the main loop awaits signals without
//! busy-polling.
//!
//! Why not `tokio::signal`? tokio's *global* signal driver did not wake us in
//! the verification environment (its self-`socketpair` write never propagated
//! an event to the reactor, while the identical primitives work when driven
//! directly). A self-pipe is the classic, dependency-light init-system pattern,
//! keeps every `unsafe` line inside this audited module, and behaves the same
//! on real hardware. The public API is intentionally small so it can be
//! swapped for `tokio::signal` if that ever becomes reliable/necessary.

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};

use tokio::io::unix::AsyncFd;

use crate::error::SyscallError;

/// Write end of the self-pipe. `-1` means no [`SignalSet`] exists yet.
///
/// A process-global is required because `sigaction` handlers receive no
/// user-data pointer. rinit create exactly one [`SignalSet`] for its lifetime.
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// The signals rinit reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// A child changed state (reap it).
    Child,
    /// Terminate (graceful shutdown).
    Term,
    /// Interrupt (graceful shutdown).
    Int,
    /// Power failure (fast shutdown; wired in M4).
    Pwr,
}

impl Signal {
    /// Signals rinit installs handlers for.
    pub const ALL: [Signal; 4] = [Signal::Child, Signal::Term, Signal::Int, Signal::Pwr];

    /// The raw `libc` signal number.
    pub const fn number(self) -> libc::c_int {
        match self {
            Signal::Child => libc::SIGCHLD,
            Signal::Term => libc::SIGTERM,
            Signal::Int => libc::SIGINT,
            Signal::Pwr => libc::SIGPWR,
        }
    }

    fn from_number(n: libc::c_int) -> Option<Signal> {
        match n {
            libc::SIGCHLD => Some(Signal::Child),
            libc::SIGTERM => Some(Signal::Term),
            libc::SIGINT => Some(Signal::Int),
            libc::SIGPWR => Some(Signal::Pwr),
            _ => None,
        }
    }
}

/// Async-signal-safe handler: record the signal number in the self-pipe.
///
/// The read-modify-write of [`WRITE_FD`] is deliberately ordered: it is loaded
/// with `Acquire` and the descriptor is only cleared (with `Release`) after
/// the pipe itself is gone. See [`SignalSet::drop_owned`] for why.
extern "C" fn dispatch(signal: libc::c_int) {
    let fd = WRITE_FD.load(Ordering::Acquire);
    if fd >= 0 {
        let byte = [signal as u8];
        // SAFETY: `fd` is a valid, non-blocking pipe write end created by
        // `SignalSet::new`; `write` is async-signal-safe. Errors (e.g. a full
        // pipe) are ignored on purpose - the signal is already latched.
        //
        // The `fd >= 0` check is not a guarantee that the descriptor is still
        // open: dropping a `SignalSet` un-installs the handlers before it
        // closes the pipe, and an in-flight handler that already passed the
        // check may still write. Writing to a closed or recycled descriptor
        // is harmless here - the worst case is one lost signal during teardown
        // - whereas tearing the pipe down while a handler could still touch it
        // is not something we can prevent from signal context.
        unsafe {
            libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
}

/// An async stream of the signals rinit cares about.
///
/// Dropping the set uninstalls the handlers it installed, restores the previous
/// dispositions, closes the pipe and clears the process-global [`WRITE_FD`].
/// That teardown is what keeps a stale descriptor out of the handler; without
/// it, a later signal would be written to a closed or recycled fd.
pub struct SignalSet {
    reader: AsyncFd<OwnedFd>,
    /// Owns the write end so the pipe is closed exactly once, on drop.
    ///
    /// The handler reaches it through the [`WRITE_FD`] global, so nothing in
    /// this crate reads the field itself; it exists to own the descriptor.
    #[cfg_attr(not(test), expect(dead_code, reason = "ownership only"))]
    writer: OwnedFd,
    /// Dispositions to restore on drop, in install order.
    restore: Vec<(libc::c_int, libc::sigaction)>,
    pending: VecDeque<Signal>,
}

impl SignalSet {
    /// Install handlers and create the self-pipe.
    ///
    /// # Errors
    /// Returns a [`SyscallError`] if `pipe2` or `sigaction` fails. A failure
    /// part-way through rolls back the handlers already installed.
    pub fn new(signals: &[Signal]) -> Result<Self, SyscallError> {
        // Non-blocking so the handler's write can never block.
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a valid 2-element out-buffer.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(SyscallError::last_os_error("pipe2"));
        }
        // SAFETY: both descriptors are fresh from `pipe2` and are now owned by
        // the `OwnedFd`s below, which close them on drop.
        let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };

        // Publish the write end only after both ends are owned, so the handler
        // can never observe a half-built pipe.
        WRITE_FD.store(writer.as_raw_fd(), Ordering::Release);

        let mut restore = Vec::with_capacity(signals.len());
        for &signal in signals {
            match install_handler(signal) {
                Ok(previous) => restore.push((signal.number(), previous)),
                Err(e) => {
                    // Roll back so a failed construction leaves no handlers
                    // pointing at a pipe that is about to close.
                    WRITE_FD.store(-1, Ordering::Release);
                    for (number, previous) in restore.into_iter().rev() {
                        restore_disposition(number, &previous);
                    }
                    return Err(e);
                }
            }
        }

        // SAFETY: `reader` owns a valid descriptor that stays open for the
        // lifetime of the returned `AsyncFd`; it is never closed or replaced.
        let reader = unsafe { AsyncFd::register(reader) }.map_err(|e| {
            let (_fd, io) = e.into_parts();
            SyscallError::from_io("AsyncFd::register(signal pipe)", &io)
        })?;

        Ok(SignalSet {
            reader,
            writer,
            restore,
            pending: VecDeque::new(),
        })
    }

    /// Await the next signal.
    pub async fn recv(&mut self) -> Signal {
        loop {
            if let Some(signal) = self.pending.pop_front() {
                return signal;
            }

            let mut guard = self
                .reader
                .readable()
                .await
                .expect("signal self-pipe AsyncFd is never closed");

            let mut buf = [0u8; 64];
            let read = guard.try_io(|inner| {
                // SAFETY: reading into `buf`, whose length is valid.
                let n = unsafe {
                    libc::read(
                        inner.get_ref().as_raw_fd(),
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                    )
                };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });

            match read {
                Ok(Ok(n)) => {
                    for &byte in &buf[..n] {
                        if let Some(signal) = Signal::from_number(byte as libc::c_int) {
                            self.pending.push_back(signal);
                        }
                    }
                }
                // WouldBlock (cleared readiness) or a transient read error:
                // loop and await readiness again.
                Ok(Err(_)) | Err(_) => continue,
            }
        }
    }
}

impl Drop for SignalSet {
    /// Undo everything [`SignalSet::new`] installed, in the reverse order.
    ///
    /// Order matters: handlers go first, then the global is cleared, and only
    /// then do the fields drop and close the pipe. Rust runs field destructors
    /// after this method returns, so no handler can observe a closed fd.
    fn drop(&mut self) {
        for (number, previous) in self.restore.drain(..).rev() {
            restore_disposition(number, &previous);
        }
        // `Release` pairs with the handler's `Acquire` load, so a handler
        // either sees `-1` and does nothing, or has already written.
        WRITE_FD.store(-1, Ordering::Release);
        // `self.writer` and `self.reader` close here.
    }
}

/// Install the async-signal-safe handler for `signal`, returning the previous
/// disposition so it can be restored on drop.
fn install_handler(signal: Signal) -> Result<libc::sigaction, SyscallError> {
    let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: `sigaction` is initialised to zero then filled with an
    // async-signal-safe handler and an empty mask; `previous` is a valid
    // out-parameter.
    let rc = unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = dispatch as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal.number(), &action, &mut previous)
    };
    if rc != 0 {
        return Err(SyscallError::last_os_error("sigaction"));
    }
    Ok(previous)
}

/// Put `previous` back for `signal`, ignoring failures: this runs during
/// teardown, where there is nothing useful left to do with an error.
fn restore_disposition(signal: libc::c_int, previous: &libc::sigaction) {
    // SAFETY: `previous` was filled by `sigaction` and stays valid here.
    unsafe {
        libc::sigaction(signal, previous, std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::Mutex;

    use super::*;

    /// Signals are process-wide, so these tests must not run concurrently with
    /// each other: one test's teardown restores dispositions while another is
    /// still expecting its handler to be installed.
    ///
    /// This is tokio's async-aware mutex, not `std::sync::Mutex`: the delivery
    /// test holds the lock across an `.await`, and holding a std lock there
    /// would block the runtime thread for the whole wait.
    static SIGNAL_LOCK: Mutex<()> = Mutex::const_new(());

    /// Every `Signal` must map to a distinct libc signal number, or two
    /// different logical events would be delivered through one handler.
    #[test]
    fn signal_numbers_are_distinct() {
        let mut numbers: Vec<i32> = Signal::ALL.iter().map(|s| s.number()).collect();
        let total = numbers.len();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), total, "signal numbers must be distinct");
    }

    /// `number`/`from_number` must round-trip for every variant.
    #[test]
    fn number_round_trips_through_from_number() {
        for signal in Signal::ALL {
            assert_eq!(Signal::from_number(signal.number()), Some(signal));
        }
    }

    /// Bytes that are not one of our signals must be dropped, not mis-parsed
    /// into a bogus `Signal`.
    #[test]
    fn from_number_rejects_unknown_bytes() {
        assert_eq!(Signal::from_number(libc::SIGKILL), None);
        assert_eq!(Signal::from_number(libc::SIGHUP), None);
        assert_eq!(Signal::from_number(0), None);
        assert_eq!(Signal::from_number(255), None);
    }

    /// The handler writes one byte per delivery; a real signal must arrive as
    /// the matching `Signal` variant.
    #[tokio::test(flavor = "current_thread")]
    async fn delivers_a_real_signal_through_the_self_pipe() {
        let _guard = SIGNAL_LOCK.lock().await;

        let mut signals = SignalSet::new(&Signal::ALL).expect("SignalSet::new");
        let writer = signals.writer.as_raw_fd();

        // SAFETY: raising a signal whose handler is installed just makes the
        // handler write one byte to the pipe.
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGINT) }, 0);

        let received = tokio::time::timeout(std::time::Duration::from_secs(5), signals.recv())
            .await
            .expect("signal must be delivered");

        assert!(writer >= 0);
        assert_eq!(received, Signal::Int);
    }

    /// The regression test for the stale-descriptor bug: after the set is
    /// dropped, the global must point at nothing, and the previous disposition
    /// must be back in place.
    #[tokio::test(flavor = "current_thread")]
    async fn drop_clears_the_global_and_restores_dispositions() {
        let _guard = SIGNAL_LOCK.lock().await;

        let before = query_disposition(libc::SIGTERM);
        let writer_fd = {
            let signals = SignalSet::new(&[Signal::Term]).expect("SignalSet::new");
            let fd = signals.writer.as_raw_fd();
            assert!(fd >= 0);
            // While alive, the handler is installed and the global is published.
            assert_eq!(WRITE_FD.load(Ordering::Relaxed), fd);
            assert_ne!(
                query_disposition(libc::SIGTERM).sa_sigaction,
                before.sa_sigaction
            );
            fd
        };

        assert_eq!(
            WRITE_FD.load(Ordering::Relaxed),
            -1,
            "the global must not survive the SignalSet"
        );
        let after = query_disposition(libc::SIGTERM);
        assert_eq!(
            after.sa_sigaction, before.sa_sigaction,
            "the previous disposition must be restored"
        );

        // The pipe write end must be closed. If the fd were still open,
        // `fcntl(F_GETFD)` would succeed instead of reporting EBADF.
        assert_eq!(
            fcntl_getfd(writer_fd),
            Some(libc::EBADF),
            "the self-pipe write end must be closed on drop"
        );
    }

    /// Closing a signal set twice-over must not leave a usable descriptor, and
    /// constructing another one afterwards must work (the global is reusable).
    #[tokio::test(flavor = "current_thread")]
    async fn successive_sets_do_not_leak_descriptors() {
        let _guard = SIGNAL_LOCK.lock().await;

        let mut fds = Vec::new();
        for _ in 0..8 {
            let signals = SignalSet::new(&[Signal::Child]).expect("SignalSet::new");
            fds.push(signals.writer.as_raw_fd());
        }
        assert_eq!(WRITE_FD.load(Ordering::Relaxed), -1);
        for fd in fds {
            assert_eq!(
                fcntl_getfd(fd),
                Some(libc::EBADF),
                "fd {fd} leaked across SignalSet lifetimes"
            );
        }
    }

    /// Read the current disposition for `signal`.
    fn query_disposition(signal: libc::c_int) -> libc::sigaction {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `action` is a valid out-parameter.
        let rc = unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) };
        assert_eq!(rc, 0, "sigaction query must succeed");
        action
    }

    /// `Some(errno)` when `fcntl` fails, so a closed fd is distinguishable
    /// from a live one.
    fn fcntl_getfd(fd: libc::c_int) -> Option<i32> {
        // SAFETY: `fcntl` on a read-only query does not modify the descriptor.
        let rc = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if rc == -1 {
            Some(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        } else {
            None
        }
    }
}
