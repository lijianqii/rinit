//! Signal disposition helpers (plan §7.1).
//!
//! Handled signals (SIGCHLD/SIGTERM/...) are registered through
//! `tokio::signal` in the boot logic. These helpers are for *explicitly*
//! ignoring signals the plan says to ignore, so dispositions are unambiguous.

/// Set `signal` to be ignored (`SIG_IGN`).
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
