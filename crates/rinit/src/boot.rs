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

use std::os::fd::AsRawFd;

use rinit_core::{
    Effect, ExitOutcome, JobTarget, Request, RequestSource, ServiceKind, Status, UnitName,
    UnitState,
};
use rinit_runtime::syscall::{process, reboot, signal};
use rinit_runtime::{RuntimeError, Signal, SignalSet};

use crate::args::Config;
use crate::control::ControlServer;
use crate::log::rlog;
use crate::logs::ServiceLogs;
use crate::manager;
use crate::notify::NotifyHub;
use crate::readiness;
use crate::service::{ServiceSet, reap_and_log, shutdown, spawn_service_in_group};
use crate::unit_manager::{
    Outcome, RestartPlan, UnitManager, build_service_env, reap_into_manager,
};
use crate::watchdog::Watchdog;

/// How often the shutdown loop wakes to reap children and check `SIGKILL`
/// deadlines. Small enough that a quick exit is noticed promptly.
const SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Per-unit stop timeout under power failure (plan §7.1): essentially no time
/// to be graceful, so the budget is capped hard.
const FAST_STOP_SECS: u64 = 2;

/// Total shutdown budget under power failure (plan §7.1).
const FAST_SHUTDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Signals rinit neither acts on nor wants any default action for (plan §7.1).
///
/// `SIGCHLD`/`SIGTERM`/`SIGINT`/`SIGPWR` are handled by [`SignalSet`]; every
/// other signal that could otherwise terminate rinit is explicitly ignored, so
/// the whole disposition matrix is auditable in one place instead of relying on
/// the kernel's special treatment of PID 1.
const IGNORED_SIGNALS: &[libc::c_int] = &[
    libc::SIGWINCH,
    // Reserved for `reload` (plan §7.1, §7.6): ignored until it is implemented.
    libc::SIGHUP,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGQUIT,
    libc::SIGPIPE,
];

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

    // Plan §7.3: the hierarchy is brought up once, here, before any unit can
    // start - and after the mount that provides it. An unusable hierarchy is a
    // warning inside `prepare`, never a reason not to boot.
    let cgroup = crate::cgroup::prepare(cfg.skip_mounts, cfg.cgroup_root.clone());

    // Install signal handlers BEFORE starting anything so no SIGCHLD is lost,
    // and make the ignore set explicit (plan §7.1).
    for &signal in IGNORED_SIGNALS {
        signal::ignore(signal);
    }
    let mut signals = SignalSet::new(&Signal::ALL).map_err(RuntimeError::from)?;

    let mut services = ServiceSet::new();

    // An explicit service bypasses unit loading entirely: that is the
    // bring-up and smoke-test path, not how a real system boots.
    if cfg.service_argv.is_empty() {
        let mut notify = NotifyHub::new();
        // One shared log ring for the whole boot (plan §1.3(7)), mirrored to a
        // file if the boot asked for one. Before `start_units`, so the first
        // service's output is captured like every other line.
        let logs = ServiceLogs::new();
        if let Some(path) = &cfg.log_file {
            logs.persist_to(path);
        }
        let mut manager = start_units(&cfg, &mut notify, &logs, cgroup).await?;
        // The control socket is bound after the mounts, so `/run` exists
        // (plan §1.3(10)); a failure to bind it is only a warning.
        let mut control = ControlServer::bind(cfg.skip_mounts);
        // Plan §7.5: armed once the boot is up and the loop below owns the
        // system, and fed from that loop's health checkpoint (see `supervise`).
        // Arming it earlier would mean feeding it from the boot sequence too, and
        // a slow level - a service with a long `TimeoutStartSec=` - would then be
        // able to reset a machine that is booting perfectly well.
        let mut watchdog = Watchdog::prepare(
            cfg.skip_mounts,
            cfg.watchdog_device.clone(),
            cfg.watchdog_timeout_sec,
        );
        supervise(
            &cfg,
            &mut signals,
            &mut manager,
            &mut notify,
            &mut control,
            &logs,
            &mut watchdog,
        )
        .await;
        // Graceful shutdown disarms the hardware (plan §7.5's magic close). A
        // hung loop never gets here, and that is the mechanism.
        watchdog.disarm();
        control.disarm();
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
async fn supervise(
    cfg: &Config,
    signals: &mut SignalSet,
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    control: &mut ControlServer,
    logs: &ServiceLogs,
    watchdog: &mut Watchdog,
) {
    // Set when the shutdown was triggered by power failure, which runs under a
    // much smaller budget (plan §7.1).
    let mut fast = false;
    loop {
        // Plan §7.5's health checkpoint: the feed happens here, in the loop that
        // starts services, reaps children and answers `rctl` - so a loop that
        // stops iterating stops feeding, and the hardware resets the machine. A
        // timer thread would keep feeding a wedged init.
        watchdog.feed_if_due();
        // Service anything already due before blocking, so a zero-length timer
        // does not wait for the next unrelated wake-up.
        for unit in manager.due_restarts() {
            match start_unit(manager, notify, logs, &unit).await {
                Ok(true) => run_start_post(manager, &unit).await,
                Ok(false) => {}
                Err(e) => rlog!("error: restart of {unit} failed: {e}"),
            }
        }
        // A start that arrived while its unit was stopping (plan §1.3(13)).
        for unit in manager.due_queued_starts() {
            match start_unit(manager, notify, logs, &unit).await {
                Ok(true) => run_start_post(manager, &unit).await,
                Ok(false) => {}
                Err(e) => rlog!("error: queued start of {unit} failed: {e}"),
            }
        }
        for unit in manager.due_start_timeouts() {
            rlog!("error: {unit} did not signal readiness in time; stopping it");
        }
        for unit in manager.due_kills() {
            rlog!("error: {unit} did not stop in time; killed");
        }
        for unit in manager.due_health_probes() {
            rlog!("probing {unit}");
        }
        for unit in manager.due_probe_timeouts() {
            rlog!("warning: {unit}: health probe timed out");
        }

        // Never block past the earliest pending deadline - which includes the
        // next watchdog feed, or a pending feed would be slept through and the
        // hardware would reset a perfectly healthy init.
        let wait = next_deadline(manager).min(watchdog.next_feed_in());
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
                    fast = true;
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
            Some((request, reply)) = control.recv() => {
                let (status, text) = handle_control(cfg, manager, notify, logs, &request).await;
                if status == Status::Ok {
                    rlog!("control: {} ok", request.describe());
                } else {
                    rlog!("control: {} -> {status:?}", request.describe());
                }
                // The client may have hung up; that is not rinit's problem.
                let _ = reply.send((status, text));
            }
            _ = tokio::time::sleep(wait) => {}
        }
    }

    // Shutdown is a `ShutdownStop` request for everything the queue still
    // wants running. Going through the queue rather than stopping each unit by
    // hand is what gives shutdown its precedence (plan §1.3(11)): its source
    // outranks a dependency start, so a unit cannot be pulled back up while the
    // system is going down.
    if fast {
        manager.compress_stop_timeouts(FAST_STOP_SECS);
    }
    for line in manager.summary() {
        rlog!("shutdown: {line}");
    }
    for unit in manager.shutdown_order() {
        let effect = manager.request_stop(&unit, RequestSource::ShutdownStop);
        if let Err(e) = apply_effect(manager, notify, logs, &unit, effect).await {
            rlog!("warning: stopping {unit} failed: {e}");
        }
    }
    notify.disarm_all();
    let budget = fast.then_some(FAST_SHUTDOWN_BUDGET);
    stop_and_reap(manager, budget).await;
}

/// Bring every unit the queue still wants running down, then wait for them.
///
/// The main loop has already broken on the shutdown signal, so this is where the
/// *second* stage of a two-stage stop happens (plan §1.1, §1.3(8)):
/// `stop_unit` sent the stop action, and any unit still alive when its
/// `TimeoutStopSec` expires is `SIGKILL`ed. Without this loop a service that
/// ignores the stop would never be killed.
///
/// `budget` is the total time allowed, used for the power-failure path where
/// plan §7.1 caps the whole shutdown at a few seconds. `None` means "as long as
/// the per-unit timeouts take".
async fn stop_and_reap(manager: &mut UnitManager, budget: Option<std::time::Duration>) {
    let give_up = budget.map(crate::deadline::after);
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
            rlog!("error: {unit} did not stop in time; killed");
        }

        if let Some(deadline) = give_up
            && std::time::Instant::now() >= deadline
        {
            rlog!(
                "warning: shutdown budget spent with {} unit(s) still running",
                manager.running_count()
            );
            break;
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
    logs: &ServiceLogs,
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

    // Capture the service's output for `rctl log` (plan §1.3(7)). Losing the
    // log is not a reason to refuse to start the service, so a failure here is
    // a warning.
    let output = match logs.capture(unit) {
        Ok(fd) => Some(fd),
        Err(e) => {
            rlog!("warning: cannot capture {unit} output: {e}");
            None
        }
    };
    let spawned =
        manager.spawn_unit_with_output(unit, &env, output.as_ref().map(|fd| fd.as_raw_fd()));
    // Close rinit's copy of the write end: as long as it is open the reader
    // never sees end-of-file, and the pump task would live forever.
    drop(output);
    spawned?;
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
    logs: &ServiceLogs,
    unit: &UnitName,
) -> Result<bool, RuntimeError> {
    // Plan §7.6: a reload that changed a *running* unit left its new definition
    // waiting for exactly this moment. It is taken before anything reads the
    // config, so `ExecStartPre`, the notify socket and the spawn all see the same
    // definition.
    manager.adopt_pending_config(unit);

    // `Conflicts=` first: the other unit must be gone before this one starts
    // (plan §1.1).
    stop_conflicts(manager, unit).await;

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

    match spawn_unit(manager, notify, logs, unit) {
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
    // Plan §7.4: every `Exec*` command runs as the service user, so a
    // preparation step cannot do what the service itself is not allowed to.
    let sandbox = match crate::sandbox::resolve(&rt.config) {
        Ok(sandbox) => sandbox,
        Err(e) => {
            rlog!("error: {unit}: cannot start '{}': {e}", argv.join(" "));
            return None;
        }
    };

    // An `ExecStartPre=`/`ExecStartPost=` command runs outside the unit's
    // cgroup: preparation is not the service, and these run before the cgroup
    // is arranged (or while the service itself already fills it).
    // An `ExecStartPre=`/`ExecStartPost=` command runs *outside* the unit's
    // cgroup and without its `TTYPath=`: it is preparation, not the service.
    let pid = match spawn_service_in_group(&argv, &env, Some(&cwd), None, &sandbox, None, None) {
        Ok(pid) => pid,
        Err(e) => {
            rlog!("error: {unit}: cannot start '{}': {e}", argv.join(" "));
            return None;
        }
    };
    let limit = if timeout == 0 {
        // `TimeoutStartSec=0` means "wait forever" (plan §1.1). There is no
        // infinite `Instant`, so this is the longest deadline rinit computes -
        // the same value `deadline` clamps an absurd timeout to.
        crate::deadline::MAX_TIMEOUT
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

/// Stop everything `unit` conflicts with and wait for it to actually go.
///
/// Plan §1.1 makes `Conflicts=` mutual exclusion with "stop the other one
/// first" semantics, so the conflicting unit is waited for by its own pid -
/// which keeps the wait from stealing the exit of an unrelated unit that boot
/// is still going to wait for.
async fn stop_conflicts(manager: &mut UnitManager, unit: &UnitName) {
    for other in manager.stop_conflicts(unit) {
        rlog!("stopping {other}: it conflicts with {unit}");
        let Some(pid) = manager.get(&other).and_then(|rt| rt.pid) else {
            continue;
        };
        let timeout = manager
            .get(&other)
            .map_or(1, |rt| rt.config.timeout_stop_sec.max(1));
        let outcome =
            match readiness::wait_for_exit(pid, std::time::Duration::from_secs(timeout)).await {
                Ok(code) => exit_outcome(code),
                Err(()) => {
                    // It did not stop in time: escalate, as the stop path would.
                    let _ = process::kill_process_group(pid, libc::SIGKILL);
                    let _ = readiness::wait_for_exit(pid, std::time::Duration::from_secs(5)).await;
                    ExitOutcome::Signalled(libc::SIGKILL)
                }
            };
        manager.on_child_exit(pid, outcome);
        rlog!("{other} stopped; {unit} may start");
    }
}

/// Turn a `wait_for_exit` result into the outcome the manager records.
fn exit_outcome(code: Option<i32>) -> ExitOutcome {
    match code {
        Some(0) => ExitOutcome::Success,
        Some(code) => ExitOutcome::Failure(code),
        // We signalled it, so a missing exit code means it died on a signal.
        None => ExitOutcome::Signalled(libc::SIGTERM),
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
pub(crate) async fn apply_effect(
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    logs: &ServiceLogs,
    unit: &UnitName,
    effect: Option<Effect>,
) -> Result<bool, RuntimeError> {
    match effect {
        None | Some(Effect::None) => Ok(false),
        // `CancelBackoffAndStart` needs no extra step: taking the unit back to
        // `Starting` clears `restart_at` on its way through.
        Some(Effect::SpawnProcess | Effect::CancelBackoffAndStart) => {
            start_unit(manager, notify, logs, unit).await
        }
        Some(Effect::ClearStartLimitAndStart) => {
            manager.clear_start_limit(unit);
            start_unit(manager, notify, logs, unit).await
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

/// Carry out a control request and describe the result (plan §3 M3).
///
/// This is where a request stops being bytes and starts being a change to the
/// running system, so it lives with the manager and the notify hub rather than
/// with the socket that delivered it.
async fn handle_control(
    cfg: &Config,
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    logs: &ServiceLogs,
    request: &Request,
) -> (Status, String) {
    match request {
        Request::List | Request::Status { unit: None, .. } => {
            (Status::Ok, manager.summary().join("\n"))
        }
        Request::Status {
            unit: Some(unit),
            tail,
        } => match describe_unit(manager, unit) {
            Some(mut text) => {
                // `status UNIT --tail` appends the unit's recent log
                // (plan §1.3(7)).
                if *tail > 0 {
                    let logs_text = render_logs(logs, Some(unit), *tail);
                    if !logs_text.is_empty() {
                        text.push_str("log:\n");
                        text.push_str(&logs_text);
                        text.push('\n');
                    }
                }
                (Status::Ok, text)
            }
            None => (Status::NoSuchUnit, format!("no such unit: {unit}")),
        },
        Request::Start(unit) => control_start(manager, notify, logs, unit).await,
        Request::Stop(unit) => control_stop(manager, notify, logs, unit).await,
        Request::Restart(unit) => control_restart(manager, notify, logs, unit).await,
        Request::ResetFailed(unit) => {
            if !manager.exists(unit) {
                return (Status::NoSuchUnit, format!("no such unit: {unit}"));
            }
            manager.reset_failed(unit);
            (Status::Ok, format!("{unit}: cleared"))
        }
        Request::Log { unit, tail } => (Status::Ok, render_logs(logs, unit.as_ref(), *tail)),
        // Plan §7.6. The reply is the report: what was added, changed, removed
        // and deferred, so a reload is never a silent act.
        Request::Reload => control_reload(cfg, manager, notify, logs).await,
    }
}

/// `rctl reload`: re-read the unit directory and apply the difference.
///
/// A failure is the interesting reply: the plan requires the *old* configuration
/// to stay in force, so the message names what was wrong with the new one and
/// the running system is exactly as it was.
async fn control_reload(
    cfg: &Config,
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    logs: &ServiceLogs,
) -> (Status, String) {
    match crate::reload::reload(cfg, manager, notify, logs).await {
        Ok(report) => {
            let mut lines = report.lines();
            let summary = report.summary();
            lines.push(summary.clone());
            (Status::Ok, lines.join("\n"))
        }
        Err(e) => (
            Status::Error,
            format!("reload rejected, the running configuration is unchanged: {e}"),
        ),
    }
}

/// `rctl start`: plan §1.3(13)'s action-derivation for a user request.
async fn control_start(
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    logs: &ServiceLogs,
    unit: &UnitName,
) -> (Status, String) {
    if !manager.exists(unit) {
        // Plan §7.7's boundary: a template is a name to instantiate, not a unit,
        // and saying which is which is the whole message.
        if manager.is_template(unit) {
            return (
                Status::Error,
                format!(
                    "{unit} is a template; start an instance, e.g. {}@<name>.service",
                    unit.prefix()
                ),
            );
        }
        // An instance of a template does not exist until someone asks for it.
        if let Err(why) = manager.instantiate(unit) {
            return (Status::NoSuchUnit, format!("no such unit: {unit} ({why})"));
        }
    }
    let effect = manager.request_start(unit, RequestSource::UserStart);
    match effect {
        Some(Effect::None) => (Status::Ok, format!("{unit} is already active")),
        // Queued behind a stop in progress; the main loop will finish it.
        None => (
            Status::Ok,
            format!("{unit} is stopping; it will start afterwards"),
        ),
        Some(effect) => match apply_effect(manager, notify, logs, unit, Some(effect)).await {
            Ok(_) => (Status::Ok, format!("started {unit}")),
            Err(e) => (Status::Error, format!("cannot start {unit}: {e}")),
        },
    }
}

/// `rctl stop`.
async fn control_stop(
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    logs: &ServiceLogs,
    unit: &UnitName,
) -> (Status, String) {
    if !manager.exists(unit) {
        return (Status::NoSuchUnit, format!("no such unit: {unit}"));
    }
    let Some(rt) = manager.get(unit) else {
        return (Status::NoSuchUnit, format!("no such unit: {unit}"));
    };
    // A unit that is already down (or failed) has nothing to stop; anything
    // else - running or a completed oneshot - is a real transition.
    let was_up = !matches!(rt.state, UnitState::Inactive | UnitState::Failed(_));

    let effect = manager.request_stop(unit, RequestSource::UserStop);
    match effect {
        Some(Effect::None) if !was_up => (Status::Ok, format!("{unit} is already stopped")),
        Some(Effect::None) => (Status::Ok, format!("stopped {unit}")),
        None => (Status::Ok, format!("{unit} is already stopping")),
        Some(effect) => match apply_effect(manager, notify, logs, unit, Some(effect)).await {
            Ok(_) => (Status::Ok, format!("stopped {unit}")),
            Err(e) => (Status::Error, format!("cannot stop {unit}: {e}")),
        },
    }
}

/// `rctl restart`: stop if running, otherwise start.
///
/// A unit that has no process to restart and nothing to redo is a conflict
/// (plan §3 M3's exit code 4).
async fn control_restart(
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    logs: &ServiceLogs,
    unit: &UnitName,
) -> (Status, String) {
    if !manager.exists(unit) {
        return (Status::NoSuchUnit, format!("no such unit: {unit}"));
    }
    match manager.restart_unit(unit) {
        RestartPlan::Stopping => {
            rlog!("restarting {unit}");
            (Status::Ok, format!("restarting {unit}"))
        }
        RestartPlan::Start => control_start(manager, notify, logs, unit).await,
        RestartPlan::NotApplicable => (
            Status::Conflict,
            format!("{unit} has no process to restart"),
        ),
    }
}

/// Render buffered log lines for a `log` or `status --tail` reply.
fn render_logs(logs: &ServiceLogs, unit: Option<&UnitName>, tail: u32) -> String {
    logs.tail(unit, tail as usize)
        .iter()
        .map(|line| format!("{}: {}", line.unit, line.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A unit's detail block for `rctl status UNIT`.
fn describe_unit(manager: &UnitManager, unit: &UnitName) -> Option<String> {
    // `exists`, not `get`: a unit a reload removed is gone even while its stop
    // is still in flight (plan §7.6).
    if !manager.exists(unit) {
        return None;
    }
    let rt = manager.get(unit)?;
    let mut out = String::new();
    out.push_str(&format!("unit: {unit}\n"));
    if !rt.config.description.is_empty() {
        out.push_str(&format!("description: {}\n", rt.config.description));
    }
    match rt.pid {
        Some(pid) => out.push_str(&format!("state: {} (pid {pid})\n", rt.state)),
        None => out.push_str(&format!("state: {}\n", rt.state)),
    }
    let target = match manager.target_of(unit) {
        Some(JobTarget::Started) => "started",
        Some(JobTarget::Stopped) => "stopped",
        None => "none",
    };
    out.push_str(&format!("target: {target}\n"));
    out.push_str(&format!(
        "restarts: {}\n",
        rt.backoff.consecutive_restarts()
    ));
    if let Some(status) = &rt.status {
        out.push_str(&format!("status: {status}\n"));
    }
    if let Some(healthy) = rt.healthy {
        out.push_str(&format!("healthy: {healthy}\n"));
    }
    Some(out)
}

/// Report what a reaped exit meant to a unit.
///
/// The manager applies the state change itself as it reaps; this only narrates
/// it, so an operator watching the console can see a service come back.
fn log_outcome(outcome: &Outcome) {
    match outcome {
        // A `forking` unit did not exit when its main process did: the daemon it
        // left behind is the unit now (plan §7.10).
        Outcome::Forked { unit, daemon } => {
            rlog!("{unit}: main process exited; daemon is pid {daemon}");
        }
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
async fn start_units(
    cfg: &Config,
    notify: &mut NotifyHub,
    logs: &ServiceLogs,
    cgroup: crate::cgroup::Cgroup,
) -> Result<UnitManager, RuntimeError> {
    let plan = manager::announce_plan(&cfg.unit_dir, &cfg.default_target)?;
    let mut units = UnitManager::new();
    // Before any unit starts: limits must be in place from the first fork.
    units.set_cgroup(cgroup);
    for cfg in &plan.configs {
        units.add(cfg.clone());
    }
    // Plan §7.7: the templates, so `rctl start worker@1.service` can make one.
    units.set_templates(plan.templates.clone());
    // Shutdown walks the startup order backwards (plan §1.3(8)), so record it
    // while the schedule is still at hand.
    units.set_shutdown_order(plan.levels.iter().flatten().cloned().collect());

    let mut started_units = 0usize;
    for (index, level) in plan.levels.iter().enumerate() {
        let batch = manager::commands_in_level(&plan, level);
        rlog!("starting level {index}: {} service(s)", batch.len());

        // Units whose *start* is not over when their process is exec'd: a
        // `oneshot` finishes by exiting, a `forking` unit by its main process
        // exiting and its daemon being located (plan §7.10). Both are waited for
        // here, with the kind carried along because only the follow-up differs.
        let mut pending: Vec<(UnitName, libc::pid_t, u64, ServiceKind)> = Vec::new();
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
            let spawned = apply_effect(&mut units, notify, logs, name, effect).await?;
            if spawned {
                started_units += 1;
            }

            // `oneshot` and `forking` are only ready once the command has
            // exited 0; `simple` and the M2 stand-in for `notify` are ready at
            // exec.
            if readiness::readiness_of(entry.kind) == readiness::Readiness::OnExitSuccess
                && let Some(pid) = units.get(name).and_then(|rt| rt.pid)
            {
                pending.push((name.clone(), pid, entry.timeout_start_sec, entry.kind));
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
        for (name, pid, timeout_sec, kind) in &pending {
            let timeout = if *timeout_sec == 0 {
                // As above: `0` is "wait forever", spelled as the longest
                // deadline rinit will compute.
                crate::deadline::MAX_TIMEOUT
            } else {
                std::time::Duration::from_secs(*timeout_sec)
            };
            match readiness::wait_for_exit(*pid, timeout).await {
                Ok(Some(0)) => match kind {
                    // Plan §7.10: the exit is the middle of a `forking` unit's
                    // start, so the daemon it left behind is what makes it
                    // `Active` - and not being able to find it fails the start
                    // rather than leaving a unit that claims to be running.
                    ServiceKind::Forking => match units.finish_forking_start(name) {
                        Ok(daemon) => {
                            rlog!("{name}: main process exited; daemon is pid {daemon}");
                            run_start_post(&units, name).await;
                        }
                        Err(why) => {
                            rlog!("error: {name}: cannot locate the daemon: {why}");
                            fail_unit(&mut units, name);
                        }
                    },
                    _ => {
                        rlog!("{name} completed (Active(Exited))");
                        units.mark_oneshot_done(name);
                        run_start_post(&units, name).await;
                    }
                },
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
                    // And whatever the process group cannot reach: a `forking`
                    // unit that never detached has already started the daemon it
                    // never named, and abandoning the start must not leave it
                    // running (plan §7.10).
                    units.signal_cgroup_members(name, libc::SIGKILL);
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
    // Filesystems first, then one final `sync`, so data on anything but the
    // root is flushed before the reset (plan §1.3(8)). Best effort: the
    // shutdown must reach `reboot(2)` even if a mount refuses to go.
    let report = rinit_runtime::unmount_filesystems();
    for point in &report.unmounted {
        rlog!("unmounted {point}");
    }
    for warning in &report.warnings {
        rlog!("warning: {warning}");
    }
    reboot::sync();
    if let Err(e) = reboot::reboot(reboot::RB_AUTOBOOT) {
        // `reboot(2)` failed. The kernel's SysRq trigger is the last resort, so
        // PID 1 never ends up in the "cannot shut down" state plan §1.3(8)
        // forbids.
        rlog!("reboot failed: {e}; trying sysrq");
        if let Err(e) = reboot::sysrq_reboot() {
            rlog!("sysrq reset failed: {e}; halting");
        }
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
    fatal_with(err, reboot_now)
}

/// The reset action a fatal early-boot error takes.
///
/// A function pointer rather than a direct call to [`reboot_now`] so the policy
/// (a fatal early error resets the board and never returns) is testable without
/// rebooting the machine.
pub type Reboofer = fn() -> !;

/// [`fatal`] with the reset action injected.
pub fn fatal_with(err: &RuntimeError, reboot: Reboofer) -> ! {
    rlog!("FATAL: {err}");
    reboot()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit_manager::RestartPlan;
    use rinit_core::UnitConfig;
    use rinit_core::UnitState;
    use rinit_core::unit::config::RestartPolicy;

    /// Plan §1.3(9): a fatal early-boot error must *reset the machine*, not
    /// return or hang - the abort path that a failed first-wave mount takes.
    #[test]
    #[should_panic(expected = "reboot was invoked")]
    fn a_fatal_boot_error_resets_the_machine() {
        let err = RuntimeError::Io {
            op: "mount",
            source: std::io::Error::other("mount /proc failed"),
        };
        fatal_with(&err, || panic!("reboot was invoked"));
    }

    // --- control dispatch (plan §3 M3) ----------------------------------

    fn unit(name: &str) -> UnitName {
        UnitName::parse(name).expect("valid name")
    }

    fn manager_with(name: &str) -> UnitManager {
        let mut cfg = UnitConfig::new(unit(name));
        cfg.exec_start =
            rinit_core::ExecCommand::new(vec!["/bin/sleep".to_string(), "30".to_string()]);
        cfg.restart = RestartPolicy::No;
        let mut m = UnitManager::new();
        m.add(cfg);
        m
    }

    /// A hub with its own scratch directory: these tests never touch `/run`.
    fn test_hub(tag: &str) -> NotifyHub {
        let dir = std::env::temp_dir().join(format!("rinit-control-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        NotifyHub::new_at(dir)
    }

    /// A boot config for these tests: the real defaults, with a unit directory
    /// that exists but is not used - they drive the manager directly.
    fn test_config() -> Config {
        let mut cfg = crate::args::parse(std::iter::empty::<std::ffi::OsString>())
            .expect("the empty command line parses");
        cfg.unit_dir = std::env::temp_dir().join("rinit-control-tests");
        cfg.default_target = "multi-user.target".to_string();
        cfg
    }

    fn status_request(unit: &UnitName) -> Request {
        Request::Status {
            unit: Some(unit.clone()),
            tail: 0,
        }
    }

    /// A request naming a unit rinit does not know is `NoSuchUnit` (exit 3),
    /// not a generic error.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unknown_unit_is_no_such_unit() {
        let _guard = crate::test_support::children_lock_async().await;
        let mut manager = UnitManager::new();
        let mut notify = test_hub("unknown");
        let logs = ServiceLogs::new();

        let ghost = unit("ghost.service");
        for request in [
            status_request(&ghost),
            Request::Start(ghost.clone()),
            Request::Stop(ghost.clone()),
            Request::Restart(ghost.clone()),
            Request::ResetFailed(ghost.clone()),
        ] {
            let (status, text) =
                handle_control(&test_config(), &mut manager, &mut notify, &logs, &request).await;
            assert_eq!(status, Status::NoSuchUnit, "{}: {text}", request.describe());
        }
    }

    /// The full `start` -> `status` -> `stop` path, and the state each reports.
    #[tokio::test(flavor = "current_thread")]
    async fn start_status_and_stop_drive_the_unit() {
        let _guard = crate::test_support::children_lock_async().await;
        let mut manager = manager_with("app.service");
        let mut notify = test_hub("startstop");
        let logs = ServiceLogs::new();
        let app = unit("app.service");

        let (status, _) = handle_control(
            &test_config(),
            &mut manager,
            &mut notify,
            &logs,
            &Request::Start(app.clone()),
        )
        .await;
        assert_eq!(status, Status::Ok);
        assert!(
            manager.get(&app).unwrap().pid.is_some(),
            "the unit must run"
        );

        let (status, text) = handle_control(
            &test_config(),
            &mut manager,
            &mut notify,
            &logs,
            &status_request(&app),
        )
        .await;
        assert_eq!(status, Status::Ok);
        assert!(text.contains("app.service"), "{text}");
        assert!(text.contains("active"), "{text}");
        assert!(text.contains("target: started"), "{text}");

        let (status, text) = handle_control(
            &test_config(),
            &mut manager,
            &mut notify,
            &logs,
            &Request::List,
        )
        .await;
        assert_eq!(status, Status::Ok);
        assert!(text.contains("app.service"), "{text}");

        let (status, _) = handle_control(
            &test_config(),
            &mut manager,
            &mut notify,
            &logs,
            &Request::Stop(app.clone()),
        )
        .await;
        assert_eq!(status, Status::Ok);
        assert_eq!(manager.get(&app).unwrap().state, UnitState::Deactivating);

        // Leave nothing running.
        let pid = manager.get(&app).unwrap().pid.expect("pid");
        let _ = process::kill_process_group(pid, libc::SIGKILL);
        let _ = rinit_runtime::syscall::wait::reap_children();
    }

    /// A second `start` is satisfied, not an error.
    #[tokio::test(flavor = "current_thread")]
    async fn starting_an_active_unit_is_satisfied() {
        let _guard = crate::test_support::children_lock_async().await;
        let mut manager = manager_with("app.service");
        let mut notify = test_hub("already");
        let logs = ServiceLogs::new();
        let app = unit("app.service");

        handle_control(
            &test_config(),
            &mut manager,
            &mut notify,
            &logs,
            &Request::Start(app.clone()),
        )
        .await;
        let (status, text) = handle_control(
            &test_config(),
            &mut manager,
            &mut notify,
            &logs,
            &Request::Start(app.clone()),
        )
        .await;
        assert_eq!(status, Status::Ok);
        assert!(text.contains("already active"), "{text}");

        let pid = manager.get(&app).unwrap().pid.expect("pid");
        let _ = process::kill_process_group(pid, libc::SIGKILL);
        let _ = rinit_runtime::syscall::wait::reap_children();
    }

    /// Plan §3 M3: `restart` of a unit with no process to restart is a
    /// conflict (exit 4).
    #[tokio::test(flavor = "current_thread")]
    async fn restart_of_a_completed_oneshot_is_a_conflict() {
        let _guard = crate::test_support::children_lock_async().await;
        let mut cfg = UnitConfig::new(unit("once.service"));
        cfg.service_kind = ServiceKind::Oneshot;
        cfg.exec_start = rinit_core::ExecCommand::new(vec!["/bin/true".to_string()]);
        let mut manager = UnitManager::new();
        manager.add(cfg);
        let mut notify = test_hub("conflict");
        let logs = ServiceLogs::new();

        // A completed oneshot has no process and nothing to redo.
        manager.mark_oneshot_done(&unit("once.service"));
        assert_eq!(
            manager.restart_unit(&unit("once.service")),
            RestartPlan::NotApplicable
        );

        let (status, text) = handle_control(
            &test_config(),
            &mut manager,
            &mut notify,
            &logs,
            &Request::Restart(unit("once.service")),
        )
        .await;
        assert_eq!(status, Status::Conflict);
        assert_eq!(status.exit_code(), 4);
        assert!(text.contains("no process to restart"), "{text}");
    }

    /// `rctl log` renders what was captured for a unit (plan §1.3(7)).
    #[tokio::test(flavor = "current_thread")]
    async fn the_log_command_renders_captured_output() {
        let _guard = crate::test_support::children_lock_async().await;
        let mut manager = UnitManager::new();
        let mut notify = test_hub("logcmd");
        let logs = ServiceLogs::new();
        let app = unit("app.service");

        // Capture and write, exactly as a spawned service's stdout would.
        let writer = logs.capture(&app).expect("capture");
        {
            use std::io::Write;
            let mut file = std::fs::File::from(writer);
            file.write_all(b"hello from the unit\n").expect("write");
        }
        for _ in 0..100 {
            if !logs.tail(Some(&app), 0).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let (status, text) = handle_control(
            &test_config(),
            &mut manager,
            &mut notify,
            &logs,
            &Request::Log {
                unit: None,
                tail: 10,
            },
        )
        .await;
        assert_eq!(status, Status::Ok);
        assert!(
            text.contains("app.service: hello from the unit"),
            "got: {text}"
        );
    }

    /// `reset-failed` clears the failure without starting the unit
    /// (plan §1.3(13)).
    #[tokio::test(flavor = "current_thread")]
    async fn reset_failed_clears_without_starting() {
        let _guard = crate::test_support::children_lock_async().await;
        let mut manager = manager_with("app.service");
        let mut notify = test_hub("reset");
        let logs = ServiceLogs::new();
        let app = unit("app.service");
        manager.fail_start(&app);
        assert!(manager.is_failed(&app));

        let (status, _) = handle_control(
            &test_config(),
            &mut manager,
            &mut notify,
            &logs,
            &Request::ResetFailed(app.clone()),
        )
        .await;
        assert_eq!(status, Status::Ok);
        assert_eq!(manager.get(&app).unwrap().state, UnitState::Inactive);
        assert!(
            manager.get(&app).unwrap().pid.is_none(),
            "it must not start"
        );
    }
}
