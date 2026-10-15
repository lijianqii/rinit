//! Service supervision: launching, tracking, reaping and stopping children
//! (plan §1.3(6)).
//!
//! M0 supervises exactly one hardcoded service. The split exists so M1/M2 can
//! grow a real unit manager here without touching the boot sequence: the
//! [`ServiceSet`] is the only thing that knows about PIDs.

use std::ffi::CString;

use rinit_runtime::RuntimeError;
use rinit_runtime::syscall::wait::ChildExit;
use rinit_runtime::syscall::{cstring, process, wait};

use crate::log::rlog;

/// Environment handed to services. rinit's own environment (from the kernel
/// cmdline) is not meaningful, so services get a fixed, predictable set.
const SERVICE_ENV: &[&str] = &["PATH=/sbin:/usr/sbin:/bin:/usr/bin", "HOME=/", "TERM=linux"];

/// How long services get to exit after `SIGTERM` before rinit stops waiting.
///
/// Plan §4.2 uses `TimeoutStopSec=` per unit; M0 has a single global value.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// The set of children rinit is responsible for.
#[derive(Debug, Default)]
pub struct ServiceSet {
    pids: Vec<libc::pid_t>,
}

impl ServiceSet {
    /// An empty set.
    pub const fn new() -> Self {
        ServiceSet { pids: Vec::new() }
    }

    /// The pids currently supervised.
    pub fn pids(&self) -> &[libc::pid_t] {
        &self.pids
    }

    /// Launch `argv` and track the resulting child.
    ///
    /// # Errors
    /// Returns a [`RuntimeError`] if the argv or env contains an interior NUL
    /// byte, or if `posix_spawn` fails.
    pub fn spawn(&mut self, argv: &[String]) -> Result<libc::pid_t, RuntimeError> {
        let pid = spawn_service(argv)?;
        self.pids.push(pid);
        Ok(pid)
    }

    /// Stop tracking `pid`, e.g. once it has been reaped.
    pub fn forget(&mut self, pid: libc::pid_t) {
        self.pids.retain(|&p| p != pid);
    }
}

/// Launch `argv` with [`SERVICE_ENV`], in a fresh process group.
///
/// Every service goes through this rather than [`spawn_service`], so that
/// stopping a unit signals its whole process group and reaches children the
/// service forked itself (plan §1.3(6)).
///
/// # Errors
/// `argv` must be non-empty and contain no interior NUL bytes.
pub fn spawn_service_in_group(argv: &[String]) -> Result<libc::pid_t, RuntimeError> {
    let (argv, envp) = prepare(argv)?;
    let path = argv[0].as_c_str();
    process::posix_spawn_with(
        path,
        &argv,
        &envp,
        process::SpawnOptions::with_process_group(),
    )
    .map_err(RuntimeError::from)
}

/// Launch `argv` with [`SERVICE_ENV`] in rinit's own process group.
///
/// Kept for bring-up and tests that need the child to share rinit's group.
///
/// # Errors
/// `argv` must be non-empty and contain no interior NUL bytes.
pub fn spawn_service(argv: &[String]) -> Result<libc::pid_t, RuntimeError> {
    let (argv, envp) = prepare(argv)?;
    let path = argv[0].as_c_str();
    process::posix_spawn(path, &argv, &envp).map_err(RuntimeError::from)
}

/// Turn `argv` into the C forms `posix_spawn` needs, plus the resolved path.
fn prepare(argv: &[String]) -> Result<(Vec<CString>, Vec<CString>), RuntimeError> {
    // `argv[0]` must be included for posix_spawn (it becomes the program name).
    let argv: Vec<CString> = argv
        .iter()
        .map(|a| cstring("service argv", a))
        .collect::<Result<_, _>>()
        .map_err(RuntimeError::from)?;
    if argv.is_empty() {
        return Err(RuntimeError::Io {
            op: "service argv",
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"),
        });
    }
    let envp: Vec<CString> = SERVICE_ENV
        .iter()
        .map(|e| cstring("service env", e))
        .collect::<Result<_, _>>()
        .map_err(RuntimeError::from)?;
    Ok((argv, envp))
}

/// Drain every exited child and log it. Returns how many were reaped.
///
/// This is safe to call at any time: `waitpid(-1, WNOHANG)` collects *all*
/// children, including adopted orphans, which is what PID 1 must do
/// (plan §5 risk 2).
pub fn reap_and_log(services: &mut ServiceSet) -> usize {
    let reaped = wait::reap_children();
    for ev in &reaped {
        match *ev {
            ChildExit::Exited { pid, code } => {
                rlog!("reaped child pid {pid} (exit {code})");
                services.forget(pid);
            }
            ChildExit::Signaled { pid, signal } => {
                rlog!("reaped child pid {pid} (killed by signal {signal})");
                services.forget(pid);
            }
        }
    }
    reaped.len()
}

/// Stop every supervised service and, when running as PID 1, reboot.
///
/// # Errors
/// Propagates a failure from the reaping step. The `SIGTERM` deliveries are
/// best-effort by design: `ESRCH` just means the service was already gone.
pub async fn shutdown(services: &mut ServiceSet) -> Result<(), RuntimeError> {
    rlog!("shutting down");

    for &pid in services.pids() {
        if let Err(e) = process::kill(pid, libc::SIGTERM) {
            // ESRCH simply means the service already exited; not fatal.
            if e.errno() != libc::ESRCH {
                rlog!("warning: SIGTERM to pid {pid} failed: {e}");
            }
        }
    }

    // Give services a brief moment to exit, then reap what we can. This must be
    // `tokio::time::sleep`, not `thread::sleep`: rinit runs a single-threaded
    // runtime, and blocking the thread would stop the reactor that delivers the
    // SIGCHLDs arriving during this window (plan §2.1①).
    tokio::time::sleep(STOP_GRACE).await;
    reap_and_log(services);

    if crate::boot::is_pid1() {
        rlog!("rebooting");
        crate::boot::reboot_now();
    } else {
        rlog!("not PID 1: stopping without reboot");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_argv_is_rejected_before_any_syscall() {
        let err = spawn_service(&[]).expect_err("empty argv must not spawn");
        assert!(err.to_string().contains("empty command"), "got: {err}");
    }

    /// An interior NUL would truncate the C string silently, so it must be
    /// refused rather than turned into a bogus path.
    #[test]
    fn interior_nul_in_argv_is_rejected() {
        let err =
            spawn_service(&["/bin/sh\0".to_string()]).expect_err("interior NUL must be rejected");
        assert!(
            matches!(err, RuntimeError::Syscall(ref e) if e.errno() == libc::EINVAL),
            "got: {err:?}"
        );
    }

    #[test]
    fn interior_nul_in_a_later_argv_element_is_rejected() {
        let err = spawn_service(&["/bin/sh".to_string(), "-c\0".to_string()])
            .expect_err("interior NUL must be rejected");
        assert!(
            matches!(err, RuntimeError::Syscall(ref e) if e.errno() == libc::EINVAL),
            "got: {err:?}"
        );
    }

    #[test]
    fn spawn_tracks_the_child_and_forget_removes_it() {
        let mut services = ServiceSet::new();
        let pid = services
            .spawn(&["/bin/sleep".to_string(), "5".to_string()])
            .expect("spawn");
        assert!(pid > 0);
        assert_eq!(services.pids(), [pid]);

        services.forget(pid);
        assert!(services.pids().is_empty());
    }

    #[test]
    fn spawning_a_missing_binary_reports_the_errno() {
        let mut services = ServiceSet::new();
        let err = services
            .spawn(&["/definitely/not/a/real/binary".to_string()])
            .expect_err("missing binary must fail");
        assert!(matches!(err, RuntimeError::Syscall(_)), "got: {err:?}");
    }

    /// Reaping must both log and untrack; a reaped pid left in the set would
    /// be SIGTERMed again during shutdown.
    #[test]
    fn reaping_untracks_the_child() {
        let mut services = ServiceSet::new();
        let pid = services
            .spawn(&[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exit 0".to_string(),
            ])
            .expect("spawn");
        assert_eq!(services.pids(), [pid]);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut reaped = 0;
        while reaped == 0 {
            reaped = reap_and_log(&mut services);
            if std::time::Instant::now() > deadline {
                panic!("child {pid} was never reaped");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            services.pids().is_empty(),
            "a reaped child must not stay supervised: {:?}",
            services.pids()
        );
    }

    /// In dev mode (not PID 1) shutdown must return instead of rebooting the
    /// developer's machine. The smoke test relies on this.
    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_is_a_noop_on_an_empty_set() {
        let mut services = ServiceSet::new();
        shutdown(&mut services)
            .await
            .expect("shutdown must not fail");
    }
}
