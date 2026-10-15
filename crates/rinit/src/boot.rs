//! Boot sequence and main event loop (plan §3 M0, §2.1).
//!
//! M0 does the minimum that proves the PID 1 skeleton works: mount the first
//! wave, launch one hardcoded service, reap children on `SIGCHLD`, and handle
//! shutdown signals. There is no config parsing or dependency graph yet.

use std::ffi::CString;
use std::time::Duration;

use rinit_runtime::syscall::wait::ChildExit;
use rinit_runtime::syscall::{process, reboot, signal, wait};
use rinit_runtime::{RuntimeError, Signal, SignalSet};

use crate::args::Config;
use crate::log::rlog;

/// Environment handed to services. rinit's own environment (from the kernel
/// cmdline) is not meaningful, so the M0 services get a fixed, predictable set.
const SERVICE_ENV: &[&str] = &["PATH=/sbin:/usr/sbin:/bin:/usr/bin", "HOME=/", "TERM=linux"];

fn cstr(op: &'static str, s: &str) -> Result<CString, RuntimeError> {
    CString::new(s).map_err(|_| RuntimeError::Io {
        op,
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "interior NUL byte"),
    })
}

/// Launch the hardcoded service from an argv vector.
fn spawn_service(argv: &[String]) -> Result<libc::pid_t, RuntimeError> {
    // `argv[0]` must be included for posix_spawn (it becomes the program name).
    let argv: Vec<CString> = argv
        .iter()
        .map(|a| cstr("service argv", a))
        .collect::<Result<_, _>>()?;
    let path = argv.first().ok_or_else(|| RuntimeError::Io {
        op: "service argv",
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"),
    })?;
    let envp: Vec<CString> = SERVICE_ENV
        .iter()
        .map(|e| cstr("service env", e))
        .collect::<Result<_, _>>()?;
    process::posix_spawn(path.as_c_str(), &argv, &envp).map_err(RuntimeError::from)
}

fn reap_and_log() -> usize {
    let reaped = wait::reap_children();
    for ev in &reaped {
        match *ev {
            ChildExit::Exited { pid, code } => rlog!("reaped child pid {pid} (exit {code})"),
            ChildExit::Signaled { pid, signal } => {
                rlog!("reaped child pid {pid} (killed by signal {signal})")
            }
        }
    }
    reaped.len()
}

/// Run the boot sequence and main loop. Returns `Ok` on a clean shutdown.
///
/// # Errors
/// Propagates fatal setup failures (mount, signal registration, spawn) so the
/// caller can decide between reboot and an error exit.
pub async fn run(cfg: Config) -> Result<(), RuntimeError> {
    rlog!(
        "rinit {} starting (pid {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );

    if cfg.skip_mounts {
        rlog!("dev mode: skipping first-wave mounts");
    } else {
        // Mounts block; run them off the async thread (plan §2.1⑤).
        let report = tokio::task::spawn_blocking(rinit_runtime::first_wave_mounts)
            .await
            .map_err(|e| RuntimeError::Join {
                op: "mount",
                detail: e.to_string(),
            })??;
        for point in &report.mounted {
            rlog!("mounted {point}");
        }
        for warning in &report.warnings {
            rlog!("warning: {warning}");
        }
    }

    // Install signal handlers BEFORE spawning services so no SIGCHLD is lost.
    let mut signals = SignalSet::new(&Signal::ALL).map_err(RuntimeError::from)?;

    let service_pid = spawn_service(&cfg.service_argv)?;
    rlog!(
        "launched test service '{}' as pid {service_pid}",
        cfg.service_argv.join(" ")
    );

    let services = vec![service_pid];

    loop {
        match signals.recv().await {
            Signal::Child => {
                reap_and_log();
            }
            Signal::Term => {
                rlog!("received SIGTERM");
                return shutdown(&services);
            }
            Signal::Int => {
                rlog!("received SIGINT");
                return shutdown(&services);
            }
            Signal::Pwr => {
                rlog!("received SIGPWR (power failure)");
                return shutdown(&services);
            }
        }
    }
}

/// Stop tracked services and, when running as PID 1, reboot.
fn shutdown(services: &[libc::pid_t]) -> Result<(), RuntimeError> {
    rlog!("shutting down");

    for &pid in services {
        if let Err(e) = process::kill(pid, libc::SIGTERM) {
            // ESRCH simply means the service already exited; not fatal.
            if e.errno() != libc::ESRCH {
                rlog!("warning: SIGTERM to pid {pid} failed: {e}");
            }
        }
    }

    // Give services a brief moment to exit, then reap what we can.
    std::thread::sleep(Duration::from_millis(500));
    reap_and_log();

    if is_pid1() {
        rlog!("rebooting");
        reboot::sync();
        if let Err(e) = reboot::reboot(reboot::RB_AUTOBOOT) {
            rlog!("reboot failed: {e}; halting");
            loop {
                signal::pause();
            }
        }
    } else {
        rlog!("not PID 1: stopping without reboot");
    }

    Ok(())
}

/// True when this process is PID 1.
pub fn is_pid1() -> bool {
    process::getpid() == 1
}

/// Fail closed on a fatal early-boot error (plan §1.3(9)).
///
/// As PID 1 the correct posture is to reset rather than hang silently; a real
/// reboot is attempted and, failing that, we block forever waiting for a
/// hardware watchdog to reset the board.
pub fn fatal(err: &RuntimeError) -> ! {
    rlog!("FATAL: {err}");
    if is_pid1() {
        let _ = reboot::reboot(reboot::RB_AUTOBOOT);
    }
    loop {
        signal::pause();
    }
}
