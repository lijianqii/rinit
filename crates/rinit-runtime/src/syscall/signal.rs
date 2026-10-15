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
