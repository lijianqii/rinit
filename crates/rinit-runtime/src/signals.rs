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
extern "C" fn dispatch(signal: libc::c_int) {
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [signal as u8];
        // SAFETY: `fd` is a valid, non-blocking pipe write end created by
        // `SignalSet::new`; `write` is async-signal-safe. Errors (e.g. a full
        // pipe) are ignored on purpose - the signal is already latched.
        unsafe {
            libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
}

/// An async stream of the signals rinit cares about.
pub struct SignalSet {
    reader: AsyncFd<OwnedFd>,
    pending: VecDeque<Signal>,
}

impl SignalSet {
    /// Install handlers and create the self-pipe.
    ///
    /// # Errors
    /// Returns a [`SyscallError`] if `pipe2` or `sigaction` fails.
    pub fn new(signals: &[Signal]) -> Result<Self, SyscallError> {
        // Non-blocking so the handler's write can never block.
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a valid 2-element out-buffer.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(SyscallError::last_os_error("pipe2"));
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        WRITE_FD.store(write_fd, Ordering::SeqCst);

        for &signal in signals {
            install_handler(signal)?;
        }

        // SAFETY: `read_fd` is a fresh descriptor owned by this object.
        let reader = unsafe { OwnedFd::from_raw_fd(read_fd) };
        // SAFETY: `reader` owns a valid descriptor that stays open for the
        // lifetime of the returned `AsyncFd`; it is never closed or replaced.
        let reader = unsafe { AsyncFd::register(reader) }.map_err(|e| {
            let (_fd, io) = e.into_parts();
            SyscallError::from_io("AsyncFd::register(signal pipe)", &io)
        })?;

        Ok(SignalSet {
            reader,
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

fn install_handler(signal: Signal) -> Result<(), SyscallError> {
    // SAFETY: `sigaction` is initialised to zero then filled with an
    // async-signal-safe handler and an empty mask.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = dispatch as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signal.number(), &action, std::ptr::null_mut()) != 0 {
            return Err(SyscallError::last_os_error("sigaction"));
        }
    }
    Ok(())
}
