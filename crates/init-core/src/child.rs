//! Process spawning and child reaping.

use crate::{ChildExit, ChildInfo};
use anyhow::{Context, Result};
use nix::{
    sys::wait::{waitpid, WaitPidFlag, WaitStatus},
    unistd::{fork, ForkResult, Pid},
};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::Command;
use tracing::{debug, warn};

/// Fork + exec a new service process.
///
/// This is the low-level spawn. The parent returns immediately with the child PID.
/// The child process:
/// 1. Creates a new session (setsid)
/// 2. Resets signal handlers to default
/// 3. Sets PR_SET_PDEATHSIG so it dies if PID 1 dies
/// 4. Executes the target binary
pub fn spawn_service(path: &str, args: &[String]) -> Result<ChildInfo> {
    match unsafe { fork() }.context("fork failed")? {
        ForkResult::Parent { child } => {
            let pid = child.as_raw();
            debug!(pid = pid, path = %path, "spawned child process");
            Ok(ChildInfo {
                pid,
                unit_name: String::new(),
            })
        }
        ForkResult::Child => {
            setsid_child().ok();
            reset_signals_child();

            unsafe {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            }

            close_extra_fds();

            let err = Command::new(path)
                .args(args)
                .exec();

            let mut stderr = std::io::stderr();
            let _ = writeln!(
                stderr,
                "rinit: failed to exec {}: {}",
                path, err
            );
            std::process::exit(127);
        }
    }
}

/// Reap all exited children without blocking (WNOHANG).
pub fn reap_children() -> Result<Vec<ChildExit>> {
    let mut exited = Vec::new();

    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, status)) => {
                debug!(pid = pid.as_raw(), code = status, "child exited");
                exited.push(ChildExit {
                    pid: pid.as_raw(),
                    status,
                });
            }
            Ok(WaitStatus::Signaled(pid, signal, _)) => {
                warn!(pid = pid.as_raw(), signal = signal as i32, "child killed by signal");
                exited.push(ChildExit {
                    pid: pid.as_raw(),
                    status: 128 + signal as i32,
                });
            }
            Ok(_) => continue,
            Err(nix::Error::ECHILD) => break,
            Err(e) => return Err(e).context("waitpid failed"),
        }
    }

    Ok(exited)
}

fn setsid_child() -> Result<(), nix::Error> {
    let sid = nix::unistd::setsid()?;
    debug!(sid = sid.as_raw(), "child created new session");
    Ok(())
}

fn reset_signals_child() {
    for sig in 1..=31 {
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
        }
    }
}

fn close_extra_fds() {
    // Iterate up to sysconf(_SC_OPEN_MAX), ignoring EBADF.
    // Do NOT read /proc/self/fd: read_dir itself uses an fd,
    // and closing it mid-iteration causes "Bad file descriptor" panics.
    let maxfd = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let maxfd = if maxfd > 0 { maxfd as i32 } else { 1024 };
    for fd in 3..maxfd {
        unsafe { libc::close(fd) };
    }
}
