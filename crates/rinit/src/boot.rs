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

use rinit_core::{Effect, RequestSource, ServiceKind, UnitName};
use rinit_runtime::syscall::{process, reboot, signal};
use rinit_runtime::{RuntimeError, Signal, SignalSet};

use crate::args::Config;
use crate::log::rlog;
use crate::manager;
use crate::notify::NotifyHub;
use crate::readiness;
use crate::service::{ServiceSet, reap_and_log, shutdown, spawn_service_in_group};
use crate::unit_manager::{Outcome, UnitManager, build_service_env, reap_into_manager};

/// How often the shutdown loop wakes to reap children and check `SIGKILL`
/// deadlines. Small enough that a quick exit is noticed promptly.
const SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(50);

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
        let mut notify = NotifyHub::new();
        let mut manager = start_units(&cfg, &mut notify).await?;
        supervise(&mut signals, &mut manager, &mut notify).await;
        // A dev-mode boot (`--dev`) is a host smoke test, not PID 1: returning
        // lets the test process exit instead of spinning in `reboot_now`,
        // where `reboot(2)` would fail without `CAP_SYS_BOOT`.
        if !cfg.skip_mounts {
            reboot_now();
        }
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
/// Four things can wake it: a child exited, a `notify` service signalled
/// readiness, a timer elapsed (restart backoff, start timeout, post-timeout
/// kill), or a signal arrived. The timers are what make this a loop rather than
/// a plain `recv()` - without them a service scheduled to restart in two
/// seconds would sit there until some unrelated signal arrived.
async fn supervise(signals: &mut SignalSet, manager: &mut UnitManager, notify: &mut NotifyHub) {
    loop {
        // Service anything already due before blocking, so a zero-length timer
        // does not wait for the next unrelated wake-up.
        for unit in manager.due_restarts() {
            match start_unit(manager, notify, &unit).await {
                Ok(true) => run_start_post(manager, &unit).await,
                Ok(false) => {}
                Err(e) => rlog!("error: restart of {unit} failed: {e}"),
            }
        }
        for unit in manager.due_start_timeouts() {
            rlog!("error: {unit} did not signal readiness in time; stopping it");
        }
        for unit in manager.due_kills() {
            rlog!("error: {unit} ignored SIGTERM; killed");
        }
        for unit in manager.due_health_probes() {
            rlog!("probing {unit}");
        }
        for unit in manager.due_probe_timeouts() {
            rlog!("warning: {unit}: health probe timed out");
        }

        // Never block past the earliest pending deadline.
        let wait = next_deadline(manager);
        tokio::select! {
            signal = signals.recv() => match signal {
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
            },
            Some((unit, message)) = notify.recv() => {
                if let Some(status) = &message.status {
                    rlog!("{unit}: {status}");
                }
                if manager.on_notify(&unit, &message) {
                    rlog!("{unit} is ready");
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
    }

    // Shutdown is a `ShutdownStop` request for everything the queue still
    // wants running. Going through the queue rather than stopping each unit by
    // hand is what gives shutdown its precedence (plan §1.3(11)): its source
    // outranks a dependency start, so a unit cannot be pulled back up while the
    // system is going down.
    for line in manager.summary() {
        rlog!("shutdown: {line}");
    }
    for unit in manager.started_units() {
        let effect = manager.request_stop(&unit, RequestSource::ShutdownStop);
        if let Err(e) = apply_effect(manager, notify, &unit, effect).await {
            rlog!("warning: stopping {unit} failed: {e}");
        }
    }
    notify.disarm_all();
    stop_and_reap(manager).await;
}

/// Bring every unit the queue still wants running down, then wait for them.
///
/// The main loop has already broken on the shutdown signal, so this is where the
/// *second* stage of a two-stage stop happens (plan §1.1, §1.3(8)):
/// `stop_unit` sent `SIGTERM`, and any unit still alive when its
/// `TimeoutStopSec` expires is `SIGKILL`ed. Without this loop a service that
/// ignores `SIGTERM` would never be killed.
async fn stop_and_reap(manager: &mut UnitManager) {
    loop {
        for outcome in reap_into_manager(manager) {
            log_outcome(&outcome);
        }
        if manager.running_count() == 0 {
            break;
        }

        let killed = manager.due_kills();
        let just_killed = !killed.is_empty();
        for unit in killed {
            rlog!("error: {unit} ignored SIGTERM; killed");
        }

        match manager.next_kill_in() {
            // A unit still has a stop deadline: wake then, or sooner to reap.
            Some(wait) => tokio::time::sleep(wait.min(SHUTDOWN_POLL)).await,
            // Nothing left to escalate. If we just sent `SIGKILL`, give it one
            // poll to be reaped; otherwise it is unkillable (e.g. D state) and
            // PID 1 must not hang the shutdown on it.
            None if just_killed => tokio::time::sleep(SHUTDOWN_POLL).await,
            None => {
                rlog!(
                    "warning: {} unit(s) did not exit; continuing shutdown",
                    manager.running_count()
                );
                break;
            }
        }
    }
}

/// Arm the notify socket and spawn `unit`'s `ExecStart`.
///
/// The socket must exist before the process starts, or the service would fail
/// to find `NOTIFY_SOCKET` (plan §1.3(12)).
///
/// Returns whether a process was spawned. A unit that spawns nothing (a target)
/// returns `true`: it still became active.
fn spawn_unit(
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    unit: &UnitName,
) -> Result<bool, RuntimeError> {
    let config = manager.get(unit).map(|rt| rt.config.clone());
    let env: Vec<String> = match config {
        Some(config) if config.service_kind == ServiceKind::Notify => {
            notify.arm(&config)?;
            notify.env_for(unit).into_iter().collect()
        }
        _ => Vec::new(),
    };
    manager.spawn_unit_with_env(unit, &env)?;
    Ok(manager.get(unit).is_some_and(|rt| rt.pid.is_some()))
}

/// Start `unit`: `ExecStartPre`, then `ExecStart`.
///
/// An `ExecStartPre` failure is a unit failure, not an infrastructure one
/// (plan §1.1), so it is recorded through [`fail_unit`] and reported as "did not
/// spawn". Only a genuine infrastructure failure (the notify socket) is
/// returned as an error.
///
/// `ExecStartPost` is deliberately *not* run here: for a `oneshot` it must run
/// after the command has exited, so the caller runs it at the right moment.
async fn start_unit(
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    unit: &UnitName,
) -> Result<bool, RuntimeError> {
    let pre = manager
        .get(unit)
        .map(|rt| rt.config.exec_start_pre.clone())
        .unwrap_or_default();
    for cmd in &pre {
        if run_aux_command(manager, unit, cmd).await != Some(0) {
            rlog!(
                "error: {unit}: ExecStartPre '{}' failed",
                cmd.argv().join(" ")
            );
            fail_unit(manager, unit);
            return Ok(false);
        }
    }

    match spawn_unit(manager, notify, unit) {
        Ok(spawned) => Ok(spawned),
        Err(e) => {
            // A spawn failure (missing binary, bad WorkingDirectory, ...) is a
            // unit failure too: plan §1.1 models it as a failed start, and the
            // rest of the boot must still come up.
            rlog!(
                "error: {unit}: cannot start '{}': {e}",
                exec_start_of(manager, unit)
            );
            fail_unit(manager, unit);
            Ok(false)
        }
    }
}

/// Run one of a unit's auxiliary commands to completion, as the service user.
///
/// `ExecStartPre=`/`ExecStartPost=` (plan §1.1) are best-effort from this
/// function's point of view: it returns the exit code, or `None` when the
/// command could not be started, was signalled, or timed out. The caller
/// decides whether that is fatal (it is for `ExecStartPre`) or merely logged
/// (for `ExecStartPost`).
async fn run_aux_command(
    manager: &UnitManager,
    unit: &UnitName,
    cmd: &rinit_core::ExecCommand,
) -> Option<i32> {
    let rt = manager.get(unit)?;
    let env = build_service_env(&rt.config, &[]);
    let cwd = rt.config.working_directory.clone();
    let timeout = rt.config.timeout_start_sec;
    let argv = cmd.argv().to_vec();

    let pid = match spawn_service_in_group(&argv, &env, Some(&cwd)) {
        Ok(pid) => pid,
        Err(e) => {
            rlog!("error: {unit}: cannot start '{}': {e}", argv.join(" "));
            return None;
        }
    };
    let limit = if timeout == 0 {
        std::time::Duration::from_secs(u32::MAX as u64)
    } else {
        std::time::Duration::from_secs(timeout)
    };
    match readiness::wait_for_exit(pid, limit).await {
        Ok(code) => code,
        Err(()) => {
            // Timed out: make sure it is gone before carrying on.
            let _ = process::kill_process_group(pid, libc::SIGKILL);
            let _ = readiness::wait_for_exit(pid, std::time::Duration::from_secs(5)).await;
            rlog!("error: {unit}: '{}' timed out", argv.join(" "));
            None
        }
    }
}

/// Run `unit`'s `ExecStartPost=` commands, logging (never propagating) failures.
async fn run_start_post(manager: &UnitManager, unit: &UnitName) {
    let post = manager
        .get(unit)
        .map(|rt| rt.config.exec_start_post.clone())
        .unwrap_or_default();
    for cmd in &post {
        if run_aux_command(manager, unit, cmd).await != Some(0) {
            rlog!(
                "warning: {unit}: ExecStartPost '{}' failed",
                cmd.argv().join(" ")
            );
        }
    }
}

/// Mark `unit` as failed and fail whatever hard-requires it.
fn fail_unit(manager: &mut UnitManager, unit: &UnitName) {
    for dependent in manager.fail_start(unit) {
        rlog!("error: {dependent} requires {unit}, which failed; not starting it");
    }
}

/// `unit`'s `ExecStart` for an error message, or an empty string.
fn exec_start_of(manager: &UnitManager, unit: &UnitName) -> String {
    manager
        .get(unit)
        .map(|rt| rt.config.exec_start.argv().join(" "))
        .unwrap_or_default()
}

/// Carry out what the Job queue decided for `unit` (plan §1.3(13)).
///
/// The manager owns the target, the state and the timers, so most effects are
/// delegated straight back to it. The one thing it cannot do is `exec`: the
/// notify socket lives with the async reactor, so the spawn happens here with
/// the environment that socket needs.
///
/// Returns whether a process was spawned, which is what the boot log counts.
async fn apply_effect(
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    unit: &UnitName,
    effect: Option<Effect>,
) -> Result<bool, RuntimeError> {
    match effect {
        None | Some(Effect::None) => Ok(false),
        // `CancelBackoffAndStart` needs no extra step: taking the unit back to
        // `Starting` clears `restart_at` on its way through.
        Some(Effect::SpawnProcess | Effect::CancelBackoffAndStart) => {
            start_unit(manager, notify, unit).await
        }
        Some(Effect::ClearStartLimitAndStart) => {
            manager.clear_start_limit(unit);
            start_unit(manager, notify, unit).await
        }
        // `stop_unit` cancels a pending restart too, so a stop is a stop.
        Some(Effect::SendStopSignal | Effect::CancelBackoffAndStop) => {
            manager.stop_unit(unit);
            Ok(false)
        }
        // `JobQueue::reconcile` produces neither: a backoff is armed by the
        // reaper, and a dependency failure is propagated where the graph
        // lives. Reaching one here would be a bug worth seeing.
        Some(Effect::ScheduleBackoff { .. } | Effect::PropagateFailure) => {
            rlog!("warning: unexpected effect for {unit}; ignoring");
            Ok(false)
        }
    }
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

/// How long to block before checking the timers again.
fn next_deadline(manager: &UnitManager) -> std::time::Duration {
    let soonest = [
        manager.next_restart_in(),
        manager.next_start_timeout_in(),
        manager.next_kill_in(),
        manager.next_health_in(),
        manager.next_probe_timeout_in(),
    ]
    .into_iter()
    .flatten()
    .min();
    // No pending timer: sleep for a long time and let a signal wake us.
    soonest.map_or(std::time::Duration::from_secs(3600), |d| {
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
/// A unit that fails to start does **not** abort the boot (plan §1.3(13)): it is
/// recorded as failed and everything that hard-`Requires=` it is failed with it
/// and skipped. Only a fatal setup problem is returned as an error.
///
/// # Errors
/// Returns a [`RuntimeError`] for a parse/dependency failure or an
/// infrastructure error (a notify socket that cannot be created).
async fn start_units(cfg: &Config, notify: &mut NotifyHub) -> Result<UnitManager, RuntimeError> {
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
            // A unit whose hard dependency already failed must not start
            // (plan §1.3(13)).
            if units.is_failed(name) {
                rlog!("skipping {name}: a required unit failed");
                continue;
            }

            // The boot schedule is a set of `Dependency` start requests, so a
            // unit pulled in twice is started once (plan §1.3(11)).
            let effect = units.request_start(name, RequestSource::Dependency);
            let spawned = apply_effect(&mut units, notify, name, effect).await?;
            if spawned {
                started_units += 1;
            }

            // `oneshot` is only ready once it has exited 0. `simple` and the
            // M2 stand-in for `notify` are ready at exec.
            if readiness::readiness_of(entry.kind) == readiness::Readiness::OnExitSuccess
                && let Some(pid) = units.get(name).and_then(|rt| rt.pid)
            {
                oneshots.push((name.clone(), pid, entry.timeout_start_sec));
            } else {
                // `ExecStartPost` runs once the main command is under way. For a
                // `oneshot` that is after it has exited, so it is run below.
                run_start_post(&units, name).await;
            }
        }

        // Wait for the initializers to finish before starting anything that
        // depends on them. The wait is a direct `waitpid` per pid rather than
        // the main loop, because boot ordering must not depend on an event loop
        // that has not finished starting.
        for (name, pid, timeout_sec) in &oneshots {
            let timeout = if *timeout_sec == 0 {
                std::time::Duration::from_secs(u32::MAX as u64)
            } else {
                std::time::Duration::from_secs(*timeout_sec)
            };
            match readiness::wait_for_exit(*pid, timeout).await {
                Ok(Some(0)) => {
                    rlog!("{name} completed (Active(Exited))");
                    units.mark_oneshot_done(name);
                    run_start_post(&units, name).await;
                }
                Ok(Some(code)) => {
                    rlog!("error: {name} exited {code}");
                    fail_unit(&mut units, name);
                }
                Ok(None) => {
                    rlog!("error: {name} was killed by a signal");
                    fail_unit(&mut units, name);
                }
                Err(()) => {
                    rlog!("error: {name} did not finish within {timeout_sec}s");
                    let _ = process::kill_process_group(*pid, libc::SIGKILL);
                    fail_unit(&mut units, name);
                }
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
