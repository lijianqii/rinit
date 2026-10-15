//! Child reaping (plan §1.3(6), §2.1③).
//!
//! rinit is PID 1 and reaps *all* children (including adopted orphans). The
//! only correct shape is a `waitpid(-1, WNOHANG)` loop driven by `SIGCHLD`
//! until it returns 0 (children exist, none ready) or fails with `ECHILD`.

/// A child's terminal state as observed by `waitpid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExit {
    /// Exited normally with `code`.
    Exited { pid: libc::pid_t, code: i32 },
    /// Killed by a signal.
    Signaled { pid: libc::pid_t, signal: i32 },
}

/// Drain all currently-exited children without blocking.
///
/// Returns the children reaped during this call. Stops at the first `0`
/// (children remain but none have exited) or `ECHILD` (no children left).
/// `EINTR` is retried.
pub fn reap_children() -> Vec<ChildExit> {
    let mut reaped = Vec::new();
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid out-pointer; WNOHANG makes this polling.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        match pid {
            p if p > 0 => {
                if libc::WIFEXITED(status) {
                    reaped.push(ChildExit::Exited {
                        pid: p,
                        code: libc::WEXITSTATUS(status),
                    });
                } else if libc::WIFSIGNALED(status) {
                    reaped.push(ChildExit::Signaled {
                        pid: p,
                        signal: libc::WTERMSIG(status),
                    });
                }
                // Stopped/continued states are ignored (no WUNTRACED requested).
            }
            0 => break,
            _ => {
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if errno == libc::EINTR {
                    continue;
                }
                break; // ECHILD or unrecoverable; nothing left to reap.
            }
        }
    }
    reaped
}
