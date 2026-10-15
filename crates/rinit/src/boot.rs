//! Boot sequence and main event loop (plan §3 M0–M1, §2.1).
//!
//! This module is the *orchestration* only: run the first wave of mounts,
//! resolve the unit directory into a startup plan, install signal handlers, and
//! dispatch the signals that arrive. Everything about processes lives in
//! [`crate::service`], everything about units in [`crate::manager`].
//!
//! From M1 a normal boot no longer launches a hardcoded service: it loads the
//! unit directory, resolves the dependency graph, and starts each *level* of
//! the schedule in parallel before moving on (plan §3 M1).

use rinit_runtime::syscall::{process, reboot, signal};
use rinit_runtime::{RuntimeError, Signal, SignalSet};

use crate::args::Config;
use crate::log::rlog;
use crate::manager;
use crate::readiness;
use crate::service::{ServiceSet, reap_and_log, shutdown};

/// Run the boot sequence and main loop. Returns `Ok` on a clean shutdown.
///
/// # Errors
/// Propagates fatal setup failures (mount, unit resolution, signal
/// registration, spawn) so the caller can decide between reboot and an error
/// exit.
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

    // Install signal handlers BEFORE starting anything so no SIGCHLD is lost.
    let mut signals = SignalSet::new(&Signal::ALL).map_err(RuntimeError::from)?;

    let mut services = ServiceSet::new();

    // An explicit service bypasses unit loading entirely: that is the
    // bring-up and smoke-test path, not how a real system boots.
    if cfg.service_argv.is_empty() {
        start_units(&cfg, &mut services)?;
    } else {
        let pid = services.spawn(&cfg.service_argv)?;
        rlog!(
            "launched test service '{}' as pid {pid}",
            cfg.service_argv.join(" ")
        );
    }

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

/// Resolve the unit directory and start every level of the schedule.
///
/// Levels run strictly in order, but the units *within* a level are all spawned
/// before the next level begins. That is the parallelism: a unit's
/// dependencies all sit in earlier levels, so nothing in a level waits on
/// anything else in the same level.
///
/// A unit that fails to spawn is fatal. Under M1's definition of "started"
/// (exec'd), a failure means a later level would be waiting on a dependency
/// that never came up.
fn start_units(cfg: &Config, services: &mut ServiceSet) -> Result<(), RuntimeError> {
    let plan = manager::announce_plan(&cfg.unit_dir, &cfg.default_target)?;

    let mut started_units = 0usize;
    for (index, level) in plan.levels.iter().enumerate() {
        let batch = manager::commands_in_level(&plan, level);
        rlog!("starting level {index}: {} service(s)", batch.len());

        let mut oneshots: Vec<(String, libc::pid_t, u64)> = Vec::new();
        for entry in batch {
            let name = entry.name;
            let pid = services.spawn(entry.argv).map_err(|e| RuntimeError::Io {
                op: "spawn unit",
                source: std::io::Error::other(format!("{name}: {e}")),
            })?;
            rlog!("started {name} as pid {pid}");
            started_units += 1;

            // `oneshot` is only ready once it has exited 0 (plan §1.3(3);
            // `Active(Exited)` still satisfies dependents). `simple` and the
            // M1 stand-in for `notify` are ready at exec.
            if readiness::readiness_of(entry.kind) == readiness::Readiness::OnExitSuccess {
                oneshots.push((name.canonical().to_string(), pid, entry.timeout_start_sec));
            }
        }

        if !oneshots.is_empty() {
            let failed = readiness::await_oneshots(services, &oneshots);
            if !failed.is_empty() {
                // A failed initializer means everything downstream would be
                // starting on a broken foundation, so stop here rather than
                // limping on. The message names the unit and its exit status.
                return Err(RuntimeError::Io {
                    op: "start units",
                    source: std::io::Error::other(format!(
                        "level {index}: {} failed, dependent units not started",
                        failed.join(", ")
                    )),
                });
            }
        }

        // Targets spawn no process (plan §7.2); reaching one means its
        // dependencies are up, which is what releases the next level.
        let targets: Vec<&str> = level
            .iter()
            .filter(|name| manager::config_of(&plan, name).is_some_and(|c| c.is_target()))
            .map(|name| name.canonical())
            .collect();
        if !targets.is_empty() {
            rlog!("level {index} reached: {} active", targets.join(", "));
        }
    }

    // `services.len()` would be 0 for an all-oneshot tree, since every child
    // has already exited and been forgotten; what the boot actually completed
    // is the schedule, not the surviving process list.
    rlog!(
        "boot complete: {started_units} unit(s) started from '{}' ({} still running)",
        plan.default_target,
        services.pids().len()
    );
    Ok(())
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
