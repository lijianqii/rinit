//! Child reaping (plan §1.3(6), §2.1③).
//!
//! rinit is PID 1 and reaps *all* children (including adopted orphans). The
//! only correct shape is a `waitpid(-1, WNOHANG)` loop driven by `SIGCHLD`
//! until it returns 0 (children exist, none ready) or fails with `ECHILD`.

use crate::error::SyscallError;
#[cfg(test)]
use crate::syscall::process::{kill, posix_spawn};

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
                if let Some(exit) = decode_status(p, status) {
                    reaped.push(exit);
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

/// Decode a `waitpid` status word for `pid`.
///
/// `None` means the status reports neither exit nor death by signal, which is
/// what a stop/continue report looks like; rinit never requests those (`waitpid`
/// is always called without `WUNTRACED`/`WCONTINUED`), so this is the honest
/// answer rather than inventing an outcome.
fn decode_status(pid: libc::pid_t, status: libc::c_int) -> Option<ChildExit> {
    if libc::WIFEXITED(status) {
        Some(ChildExit::Exited {
            pid,
            code: libc::WEXITSTATUS(status),
        })
    } else if libc::WIFSIGNALED(status) {
        Some(ChildExit::Signaled {
            pid,
            signal: libc::WTERMSIG(status),
        })
    } else {
        None
    }
}

/// Poll one specific child without blocking (plan §1.3(6)).
///
/// This is the single-child form of [`reap_children`], for the callers that
/// wait for a *particular* pid (a `oneshot` unit's `ExecStart`, a service that
/// must stop before its conflict can start). It lives here, next to
/// `reap_children`, so that `libc::waitpid` is called from exactly one module
/// (plan §2.3) and the status decoding above is shared.
///
/// Returns:
///
/// - `Ok(Some(exit))` once the child has terminated,
/// - `Ok(None)` while it is still running,
/// - `Err(e)` if `waitpid` itself failed. `EINTR` is the caller's to retry, and
///   `ECHILD` means the child was already reaped or never existed.
pub fn poll_child(pid: libc::pid_t) -> Result<Option<ChildExit>, SyscallError> {
    let mut status: libc::c_int = 0;
    // SAFETY: `status` is a valid out-pointer, and `WNOHANG` keeps this a poll
    // rather than a block - which is what lets the async caller honour its own
    // timeout.
    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if rc == 0 {
        return Ok(None);
    }
    if rc < 0 {
        return Err(SyscallError::last_os_error("waitpid"));
    }
    // `WNOHANG` was requested, so a positive `rc` is this child terminating.
    Ok(decode_status(pid, status))
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use super::*;

    /// `reap_children` drains every child of this process, so tests sharing it
    /// must not run concurrently.
    static REAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    /// Spawn `n` children that exit immediately, wait until all of them have
    /// become zombies, and confirm a *single* drain collects them all.
    ///
    /// This is the whole point of the loop in `reap_children`: several
    /// `SIGCHLD`s (or a single one covering many children) must not leave
    /// unreaped processes behind, which for PID 1 means zombie accumulation and
    /// eventually PID exhaustion (plan §5 risk 2).
    #[test]
    fn drains_multiple_children_in_one_pass() {
        let _guard = REAP_LOCK.lock().unwrap();
        const N: i32 = 5;

        let mut pids = Vec::new();
        for code in 0..N {
            let argv = [
                cs("sh"),
                cs("-c"),
                CString::new(format!("exit {code}")).unwrap(),
            ];
            pids.push(posix_spawn(c"/bin/sh", &argv, &[]).expect("posix_spawn"));
        }

        // Let every child reach the exited-but-unreaped state. Only then can a
        // single drain be expected to see them all.
        let mut collected = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while collected.len() < N as usize && std::time::Instant::now() < deadline {
            collected.extend(reap_children());
        }

        collected.sort_by_key(|ev| match *ev {
            ChildExit::Exited { pid, .. } | ChildExit::Signaled { pid, .. } => pid,
        });
        pids.sort_unstable();

        let collected_pids: Vec<libc::pid_t> = collected
            .iter()
            .map(|ev| match *ev {
                ChildExit::Exited { pid, .. } | ChildExit::Signaled { pid, .. } => pid,
            })
            .collect();
        assert_eq!(
            collected_pids, pids,
            "every spawned child must be reaped, got {collected_pids:?} vs {pids:?}"
        );

        let mut codes: Vec<i32> = collected
            .iter()
            .map(|ev| match *ev {
                ChildExit::Exited { code, .. } => code,
                ChildExit::Signaled { pid, .. } => panic!("pid {pid} died unexpectedly"),
            })
            .collect();
        codes.sort_unstable();
        assert_eq!(
            codes,
            (0..N).collect::<Vec<i32>>(),
            "exit codes must survive"
        );
    }

    /// With no children left, the loop must stop on `ECHILD` and return an
    /// empty batch rather than spinning or reporting an error.
    #[test]
    fn returns_empty_when_there_are_no_children() {
        let _guard = REAP_LOCK.lock().unwrap();
        // Drain anything left over from other tests first.
        reap_children();
        assert_eq!(
            reap_children(),
            Vec::new(),
            "no children means no events, not an error"
        );
    }

    /// A child that is still running must not be reported, and must not stop
    /// the drain from collecting a sibling that already exited.
    #[test]
    fn skips_live_children_and_still_reaps_exited_siblings() {
        let _guard = REAP_LOCK.lock().unwrap();
        reap_children();

        let live_argv = [cs("sh"), cs("-c"), cs("sleep 30")];
        let live = posix_spawn(c"/bin/sh", &live_argv, &[]).expect("posix_spawn live");

        let exit_argv = [cs("sh"), cs("-c"), cs("exit 3")];
        let exiting = posix_spawn(c"/bin/sh", &exit_argv, &[]).expect("posix_spawn exiting");

        let mut saw_exit = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !saw_exit && std::time::Instant::now() < deadline {
            for ev in reap_children() {
                match ev {
                    ChildExit::Exited { pid, code } if pid == exiting => {
                        assert_eq!(code, 3);
                        saw_exit = true;
                    }
                    ChildExit::Exited { pid, .. } | ChildExit::Signaled { pid, .. }
                        if pid == live =>
                    {
                        panic!("a running child must not be reaped")
                    }
                    _ => {}
                }
            }
        }
        assert!(saw_exit, "the exited sibling must be collected");

        // Clean up the long-lived child so it does not leak into other tests.
        kill(live, libc::SIGKILL).expect("kill");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if reap_children()
                .iter()
                .any(|ev| matches!(ev, ChildExit::Signaled { pid, .. } if *pid == live))
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("failed to reap the cleanup child {live}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
