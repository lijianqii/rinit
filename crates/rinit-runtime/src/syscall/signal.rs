//! Signal disposition helpers (plan §7.1).
//!
//! Handled signals (SIGCHLD/SIGTERM/...) are registered through
//! `tokio::signal` in the boot logic. These helpers are for *explicitly*
//! ignoring signals the plan says to ignore, so dispositions are unambiguous.

/// Set `signal` to be ignored (`SIG_IGN`).
///
/// Wired up in M4 (plan §7.1): signals rinit must not act on are explicitly
/// ignored rather than left at their default, so the disposition is auditable
/// from one place instead of being implicit in the kernel.
pub fn ignore(signal: libc::c_int) {
    // SAFETY: installing a standard disposition via `signal(2)`.
    unsafe {
        libc::signal(signal, libc::SIG_IGN);
    }
}

/// Suspend the calling thread until a signal is delivered.
pub fn pause() {
    // SAFETY: `pause` takes no arguments and has no memory-safety preconditions.
    unsafe {
        libc::pause();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ignore` must actually set `SIG_IGN`, which is what makes the
    /// disposition matrix in plan §7.1 auditable rather than implicit.
    #[test]
    fn ignore_installs_sig_ign_and_can_be_restored() {
        // A signal nothing else in the test process relies on.
        let signal = libc::SIGWINCH;

        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `previous` is a valid out-parameter.
        assert_eq!(
            unsafe { libc::sigaction(signal, std::ptr::null(), &mut previous) },
            0
        );

        ignore(signal);
        let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `current` is a valid out-parameter.
        assert_eq!(
            unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) },
            0
        );
        assert_eq!(
            current.sa_sigaction,
            libc::SIG_IGN,
            "ignore must install SIG_IGN"
        );

        // SAFETY: `previous` came from `sigaction` and stays valid here.
        unsafe {
            libc::sigaction(signal, &previous, std::ptr::null_mut());
        }
    }
}
