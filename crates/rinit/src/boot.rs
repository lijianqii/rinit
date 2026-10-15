//! Boot sequence and main event loop (plan §3 M0, §2.1).
//!
//! This module is the *orchestration* only: run the first wave of mounts,
//! install signal handlers, hand off to the supervisor, and dispatch the
//! signals that arrive. Everything about processes lives in [`crate::service`].
//!
//! M0 does the minimum that proves the PID 1 skeleton works. There is no
//! config parsing or dependency graph yet; those land in M1/M2.

use rinit_runtime::syscall::{process, reboot, signal};
use rinit_runtime::{RuntimeError, Signal, SignalSet};

use crate::args::Config;
use crate::log::rlog;
use crate::service::{ServiceSet, reap_and_log, shutdown};

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
        report_mounts().await?;
    }

    // Install signal handlers BEFORE spawning services so no SIGCHLD is lost.
    let mut signals = SignalSet::new(&Signal::ALL).map_err(RuntimeError::from)?;

    let mut services = ServiceSet::new();
    let service_pid = services.spawn(&cfg.service_argv)?;
    rlog!(
        "launched test service '{}' as pid {service_pid}",
        cfg.service_argv.join(" ")
    );

    loop {
        match signals.recv().await {
            Signal::Child => reap_and_log(&mut services),
            Signal::Term => {
                rlog!("received SIGTERM");
                return shutdown(&mut services).await;
            }
            Signal::Int => {
                rlog!("received SIGINT");
                return shutdown(&mut services).await;
            }
            Signal::Pwr => {
                rlog!("received SIGPWR (power failure)");
                return shutdown(&mut services).await;
            }
        };
    }
}

/// Run the first-wave mounts off the async thread and log the outcome.
///
/// Mounts block, so they go through `spawn_blocking` (plan §2.1⑤). This is the
/// one place permitted to block on a syscall.
async fn report_mounts() -> Result<(), RuntimeError> {
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
    Ok(())
}

/// True when this process is PID 1.
pub fn is_pid1() -> bool {
    process::getpid() == 1
}

/// Reboot the system, or block forever if that fails.
///
/// Only ever called as PID 1 (see [`is_pid1`]). As PID 1 the correct posture
/// on a failed shutdown is to reset the board rather than hang silently; a real
/// reboot is attempted and, failing that, we block waiting for a hardware
/// watchdog (plan §5 risk 1).
pub fn reboot_now() -> ! {
    rlog!("rebooting");
    reboot::sync();
    if let Err(e) = reboot::reboot(reboot::RB_AUTOBOOT) {
        rlog!("reboot failed: {e}; halting");
    }
    loop {
        signal::pause();
    }
}

/// Fail closed on a fatal early-boot error (plan §1.3(9)).
///
/// Only reachable as PID 1: `main` checks [`is_pid1`] first and reports the
/// error normally in dev mode, so this never strands a test or a developer
/// shell in an unrecoverable loop.
pub fn fatal(err: &RuntimeError) -> ! {
    rlog!("FATAL: {err}");
    reboot_now()
}
