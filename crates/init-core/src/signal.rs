//! Signal handling using signalfd.

use crate::SignalFd;
use anyhow::{Context, Result};
use nix::sys::signalfd::SignalFd as NixSignalFd;
use nix::sys::signalfd::SfdFlags;
use nix::sys::signal::SigSet;
use std::os::unix::io::{AsRawFd, RawFd};
use tracing::debug;

/// Signals that PID 1 must always handle.
pub const REQUIRED_SIGNALS: &[i32] = &[
    libc::SIGCHLD,
    libc::SIGTERM,
    libc::SIGINT,
    libc::SIGHUP,
    libc::SIGPWR,
];

/// Block default signal handlers and prevent signals from being delivered normally.
pub fn block_default_signals() -> Result<()> {
    let mut sigset = SigSet::empty();
    for &sig in REQUIRED_SIGNALS {
        sigset.add(nix::sys::signal::Signal::try_from(sig).unwrap());
    }
    sigset.thread_block().context("failed to block signals")?;
    debug!("default signals blocked for PID 1");
    Ok(())
}

/// Create a signalfd and block the specified signals.
pub fn create_signal_fd(signals: &[i32]) -> Result<SignalFd> {
    let mut sigset = SigSet::empty();
    for &sig in signals {
        sigset.add(nix::sys::signal::Signal::try_from(sig).unwrap());
    }

    let sfd = NixSignalFd::with_flags(&sigset, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC)
        .context("failed to create signalfd")?;

    let fd = sfd.as_raw_fd();
    std::mem::forget(sfd);

    debug!(fd, "signalfd created");
    Ok(SignalFd { fd })
}

/// Read pending signals from a raw signalfd file descriptor.
pub fn read_signals_from_fd(fd: &RawFd) -> Result<Vec<i32>> {
    // Use libc read directly to get signalfd_siginfo structures
    let mut signals = Vec::new();
    let mut buf = [0u8; 128]; // sizeof(signalfd_siginfo) = 128

    loop {
        let n = unsafe {
            libc::read(*fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };

        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                break;
            }
            return Err(err).context("signalfd read failed");
        }

        if n == 0 {
            break;
        }

        // Parse siginfo structures
        let count = n as usize / std::mem::size_of::<libc::signalfd_siginfo>();
        let siginfos = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr() as *const libc::signalfd_siginfo,
                count,
            )
        };

        for siginfo in siginfos {
            signals.push(siginfo.ssi_signo as i32);
        }
    }

    Ok(signals)
}
