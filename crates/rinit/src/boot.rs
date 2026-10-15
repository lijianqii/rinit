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

use rinit_core::UnitName;
use rinit_runtime::syscall::{process, reboot, signal};
use rinit_runtime::{RuntimeError, Signal, SignalSet};

use crate::args::Config;
use crate::log::rlog;
use crate::manager;
use crate::readiness;
use crate::service::{ServiceSet, reap_and_log, shutdown};
use crate::unit_manager::{Outcome, UnitManager, reap_into_manager};

/// How long to wait after SIGTERM before declaring a stop complete.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

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
        let mut manager = start_units(&cfg)?;
        supervise(&mut signals, &mut manager).await;
        return Ok(());
    }

    let pid = services.spawn(&cfg.service_argv)?;
    rlog!(
        "launched test service '{}' as pid {pid}",
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

/// The supervised main loop (plan §3 M2).
///
/// Three things can wake it: a child exited, a restart backoff elapsed, or a
/// signal arrived. The backoff timer is what makes this a loop rather than a
/// plain `recv()` - without it a service scheduled to restart in two seconds
/// would sit there until some unrelated signal arrived.
async fn supervise(signals: &mut SignalSet, manager: &mut UnitManager) {
    loop {
        // Fire any restart whose backoff has elapsed before blocking, so a
        // backoff of zero does not wait for the next signal.
        for unit in manager.due_restarts() {
            if let Err(e) = manager.spawn_unit(&unit) {
                rlog!("error: restart of {unit} failed: {e}");
            }
        }

        // Never block for longer than the next due restart.
        let wait = next_restart_delay(manager);
        let signal = tokio::select! {
            signal = signals.recv() => signal,
            _ = tokio::time::sleep(wait) => continue,
        };

        match signal {
            Signal::Child => {
                for outcome in reap_into_manager(manager) {
                    log_outcome(&outcome);
                }
            }
            Signal::Term => {
                rlog!("received SIGTERM");
                break;
            }
            Signal::Int => {
                rlog!("received SIGINT");
                break;
            }
            Signal::Pwr => {
                rlog!("received SIGPWR (power failure)");
                break;
            }
        }
    }

    // Shutdown is ordered by the Job queue in M4; for now stop every unit.
    for line in manager.summary() {
        rlog!("shutdown: {line}");
    }
    for unit in manager.units_to_stop() {
        manager.stop_unit(&unit);
    }
    tokio::time::sleep(STOP_WAIT).await;
    for outcome in reap_into_manager(manager) {
        log_outcome(&outcome);
    }
    reboot_now();
}

/// Report what a reaped exit meant to a unit.
///
/// The manager applies the state change itself as it reaps; this only narrates
/// it, so an operator watching the console can see a service come back.
fn log_outcome(outcome: &Outcome) {
    match outcome {
        Outcome::Exited { unit, state } => rlog!("{unit} exited: {state}"),
        Outcome::RestartScheduled { unit, delay } => {
            rlog!("{unit} will restart in {delay:?}");
        }
        Outcome::StartLimitTripped { unit, failures } => {
            rlog!("error: {unit} hit its start limit ({failures} failures); not restarting");
        }
    }
}

/// How long to block before checking for due restarts again.
fn next_restart_delay(manager: &UnitManager) -> std::time::Duration {
    manager
        .next_restart_in()
        .map_or(std::time::Duration::from_secs(3600), |d| {
            d.max(std::time::Duration::from_millis(1))
        })
}

/// Resolve the unit directory and start every level of the schedule.
///
/// Levels run strictly in order, but the units *within* a level are all spawned
/// before the next level begins. That is the parallelism: a unit's
/// dependencies all sit in earlier levels, so nothing in a level waits on
/// anything else in the same level.
///
/// `oneshot` units must finish before the next level starts (plan §1.3(3):
/// `Active(Exited)` is what satisfies dependents). They are waited for with a
/// blocking reap rather than the main loop, because boot ordering cannot depend
/// on an event loop that has not finished starting yet.
///
/// # Errors
/// Returns a [`RuntimeError`] if a unit fails to spawn or an initializer
/// fails: both mean a later level would be waiting on a dependency that never
/// came up.
fn start_units(cfg: &Config) -> Result<UnitManager, RuntimeError> {
    let plan = manager::announce_plan(&cfg.unit_dir, &cfg.default_target)?;
    let mut units = UnitManager::new();
    for cfg in &plan.configs {
        units.add(cfg.clone());
    }

    let mut started_units = 0usize;
    for (index, level) in plan.levels.iter().enumerate() {
        let batch = manager::commands_in_level(&plan, level);
        rlog!("starting level {index}: {} service(s)", batch.len());

        let mut oneshots: Vec<(UnitName, libc::pid_t, u64)> = Vec::new();
        for entry in batch {
            let name = entry.name;
            units.spawn_unit(name).map_err(|e| RuntimeError::Io {
                op: "spawn unit",
                source: std::io::Error::other(format!("{name}: {e}")),
            })?;
            started_units += 1;

            // `oneshot` is only ready once it has exited 0. `simple` and the
            // M2 stand-in for `notify` are ready at exec.
            if readiness::readiness_of(entry.kind) == readiness::Readiness::OnExitSuccess
                && let Some(pid) = units.get(name).and_then(|rt| rt.pid)
            {
                oneshots.push((name.clone(), pid, entry.timeout_start_sec));
            }
        }

        // Wait for the initializers to finish before starting anything that
        // depends on them. The wait is a direct `waitpid` per pid rather than
        // the main loop, because boot ordering must not depend on an event loop
        // that has not finished starting.
        let mut failed_oneshots: Vec<String> = Vec::new();
        for (name, pid, timeout_sec) in &oneshots {
            let timeout = if *timeout_sec == 0 {
                std::time::Duration::from_secs(u32::MAX as u64)
            } else {
                std::time::Duration::from_secs(*timeout_sec)
            };
            match readiness::wait_for_exit(*pid, timeout) {
                Ok(Some(0)) => {
                    rlog!("{name} completed (Active(Exited))");
                    units.mark_oneshot_done(name);
                }
                Ok(Some(code)) => {
                    rlog!("error: {name} exited {code}");
                    failed_oneshots.push(name.canonical().to_string());
                }
                Ok(None) => {
                    rlog!("error: {name} was killed by a signal");
                    failed_oneshots.push(name.canonical().to_string());
                }
                Err(()) => {
                    rlog!("error: {name} did not finish within {timeout_sec}s");
                    failed_oneshots.push(name.canonical().to_string());
                }
            }
        }
        if !failed_oneshots.is_empty() {
            // A failed initializer means everything downstream would be starting
            // on a broken foundation, so stop here rather than limping on.
            return Err(RuntimeError::Io {
                op: "start units",
                source: std::io::Error::other(format!(
                    "level {index}: {} failed, dependent units not started",
                    failed_oneshots.join(", ")
                )),
            });
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

    rlog!(
        "boot complete: {started_units} unit(s) started from '{}' ({} still running)",
        plan.default_target,
        units.running_count()
    );
    Ok(units)
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
