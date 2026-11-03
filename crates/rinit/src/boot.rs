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

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use rinit_core::{
    Effect, ExitOutcome, JobTarget, ListFormat, Request, RequestSource, ServiceKind, Status,
    StatusFormat, UnitName, UnitState,
};
use rinit_runtime::syscall::{process, reboot, signal};
use rinit_runtime::{RuntimeError, Signal, SignalSet};
use tokio::sync::oneshot;

use crate::activation::Activated;
use crate::args::Config;
use crate::control::ControlServer;
use crate::log::flush_suppressed;
use crate::log::rlog;
use crate::logs::ServiceLogs;
use crate::manager;
use crate::notify::NotifyHub;
use crate::readiness;
use crate::service::{ServiceSet, reap_and_log, shutdown, spawn_service_in_group};
use crate::unit_manager::{
    Outcome, ResourceUsage, RestartPlan, UnitListEntry, UnitManager, UnitRuntime,
    build_service_env, reap_into_manager,
};
use crate::watchdog::Watchdog;

/// How often the shutdown loop wakes to reap children and check `SIGKILL`
/// deadlines. Small enough that a quick exit is noticed promptly.
const SHUTDOWN_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// How often an otherwise idle supervisor loop wakes to drain children.
///
/// `SIGCHLD` covers a service's direct child, but PID 1 also adopts orphans
/// reparented to it, and an already-dead orphan does not necessarily generate a
/// fresh signal. Draining once a second independently of the signal closes that
/// gap without turning the loop into a busy poll.
const HOUSEKEEPING_POLL: std::time::Duration = std::time::Duration::from_secs(1);

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
    // Plan §4.3's boot measurement, in two clocks that turn out to be one:
    // `Instant` is `CLOCK_MONOTONIC`, and so is `/proc/uptime` - the difference is
    // that this one can be read *here*, before `/proc` exists, and the file cannot
    // be read until the first wave has mounted it. Taking both gives the kernel's
    // uptime when the first service started (the number the plan asks for) and
    // rinit's own share of it.
    let started = std::time::Instant::now();

    if cfg.skip_mounts {
        rlog!("dev mode: skipping first-wave mounts");
    } else {
        report_mounts().await?;
        // Plan §1.3(9): the second wave, `/etc/fstab`, after the first - it needs
        // `/proc` for the command line that can veto it, and it is the reason the
        // first wave has to be complete before anything else runs.
        report_fstab(&cfg).await?;
    }

    // Plan §7.3: the hierarchy is brought up once, here, before any unit can
    // start - and after the mount that provides it. An unusable hierarchy is a
    // warning inside `prepare`, never a reason not to boot.
    let cgroup = crate::cgroup::prepare(cfg.skip_mounts, cfg.cgroup_root.clone());

    // Install signal handlers BEFORE starting anything so no SIGCHLD is lost,
    // and make the ignore set explicit (plan §7.1).
    for &signal in IGNORED_SIGNALS {
        if let Err(e) = signal::ignore(signal) {
            rlog!("warning: cannot ignore signal {signal}: {e}");
        }
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
        // Plan §7.9: the sockets rinit holds for units that asked to be started
        // by a connection rather than at boot.
        let mut activation = Activated::disabled();
        let mut manager =
            start_units(&cfg, &mut notify, &logs, cgroup, &mut activation, started).await?;
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
        let mut supervised = Supervised {
            notify: &mut notify,
            control: &mut control,
            logs: &logs,
        };
        let shutdown = supervise(
            &cfg,
            &mut signals,
            &mut manager,
            &mut supervised,
            &mut watchdog,
            &mut activation,
        )
        .await;
        // The service pipes are closed by now; write whatever they queued before
        // the machine goes down. The ring remains the in-memory source of truth.
        logs.flush_file();
        activation.disarm();
        // Graceful shutdown disarms the hardware (plan §7.5's magic close). A
        // hung loop never gets here, and that is the mechanism.
        watchdog.disarm();
        control.disarm();
        // Whatever the console limiter folded and has not reported yet: a flood
        // that stopped leaves its count in the table, and the log would otherwise
        // end mid-sentence (see `log.rs`).
        flush_suppressed();
        // A dev-mode boot (`--dev`) is a host smoke test, not PID 1: returning
        // lets the test process exit instead of spinning in `reboot_now`,
        // where `reboot(2)` would fail without `CAP_SYS_BOOT`.
        if !cfg.skip_mounts {
            match shutdown {
                ShutdownAction::Reboot => reboot_now(),
                ShutdownAction::PowerOff => poweroff_now(),
            }
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

/// What the supervisor must do after all services have stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownAction {
    /// Restart the machine.
    Reboot,
    /// Power the machine off.
    PowerOff,
}

struct Supervised<'a> {
    /// Where a started service's readiness arrives.
    notify: &'a mut NotifyHub,
    /// Where `rctl` is answered.
    control: &'a mut ControlServer,
    /// Where services' output goes (plan §1.3(7)).
    logs: &'a ServiceLogs,
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
    supervised: &mut Supervised<'_>,
    watchdog: &mut Watchdog,
    activation: &mut Activated,
) -> ShutdownAction {
    // Set when the shutdown was triggered by power failure, which runs under a
    // much smaller budget (plan §7.1).
    let mut fast = false;
    let mut action = ShutdownAction::Reboot;
    let mut starts = StartCoordinator::default();
    // Last activation per socket, so a listener that stays readable does not
    // spawn the service on every watcher tick.
    let mut socket_activations: BTreeMap<UnitName, Instant> = BTreeMap::new();
    loop {
        // Drain every exited child before advancing starts: a `oneshot` or
        // `forking` unit's readiness is a state change the reaper makes, and an
        // `ExecStartPre=` exit has to reach its flow rather than be discarded.
        for outcome in starts.reap(manager) {
            log_outcome(&outcome);
        }
        // Advance each start one step. Nothing here waits: a slow socket, a
        // conflicting unit or a long `ExecStartPre=` returns to this loop.
        starts.advance(manager, supervised.notify, supervised.logs);
        // Plan §7.5's health checkpoint: the feed happens here, in the loop that
        // starts services, reaps children and answers `rctl` - so a loop that
        // stops iterating stops feeding, and the hardware resets the machine. A
        // timer thread would keep feeding a wedged init.
        watchdog.feed_if_due();
        // Persist one bounded batch of captured lines. The log pumps only
        // enqueue, so a slow disk can delay the file without delaying a service
        // or holding the log mutex across a write. Feeding before this keeps a
        // slow write from eating the feed's whole margin.
        supervised.logs.drain_file();
        // Service anything already due before blocking, so a zero-length timer
        // does not wait for the next unrelated wake-up.
        for unit in manager.due_restarts() {
            if let Err(e) = starts.begin(manager, &unit, StartCompletion::AtReady) {
                rlog!("error: restart of {unit} failed: {e}");
            }
        }
        // A start that arrived while its unit was stopping (plan §1.3(13)).
        for unit in manager.due_queued_starts() {
            if let Err(e) = starts.begin(manager, &unit, StartCompletion::AtReady) {
                rlog!("error: queued start of {unit} failed: {e}");
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
        // Plan §7.9's idle rule: nobody has connected for `IdleTimeoutSec=`, so
        // the service is stopped and the socket stays - which is the memory the
        // directive exists to reclaim.
        for unit in manager.due_idle_units() {
            rlog!("stopping {unit}: idle");
            starts.cancel(&unit);
            let effect = manager.request_stop(&unit, RequestSource::UserStop);
            if let Err(e) =
                apply_effect(manager, supervised.notify, supervised.logs, &unit, effect).await
            {
                rlog!("error: cannot stop idle {unit}: {e}");
            }
        }

        // Never block past the earliest pending deadline - which includes the
        // next watchdog feed, or a pending feed would be slept through and the
        // hardware would reset a perfectly healthy init - or a start flow that
        // has something to poll.
        let wait = next_deadline(manager)
            .min(watchdog.next_feed_in())
            .min(starts.next_wait());
        // Plan §7.9: a connection on an activated socket. Collected here rather
        // than handled in the `select!` arm, because the arm cannot borrow the
        // socket set while its own `recv` is one of the branches.
        let mut woke_socket: Option<UnitName> = None;
        tokio::select! {
            signal = signals.recv() => match signal {
                // The reap happens at the top of the loop, where auxiliary exits
                // can be routed to the flow waiting for them. Reaping here would
                // consume a command's status before its flow ever saw it.
                Signal::Child => {}
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
            Some((unit, message)) = supervised.notify.recv() => {
                if let Some(status) = &message.status {
                    rlog!("{unit}: {status}");
                }
                if manager.on_notify(&unit, &message) {
                    rlog!("{unit} is ready");
                }
            }
            Some((request, reply)) = supervised.control.recv() => {
                let requested = match &request {
                    Request::Reboot => Some(ShutdownAction::Reboot),
                    Request::Poweroff => Some(ShutdownAction::PowerOff),
                    _ => None,
                };
                if let Some(requested) = requested {
                    action = requested;
                }
                if let Request::Stop(unit) = &request {
                    starts.cancel(unit);
                }
                let reloaded = matches!(&request, Request::Reload);
                match handle_control(
                    cfg,
                    manager,
                    supervised.notify,
                    supervised.logs,
                    activation,
                    &mut starts,
                    &request,
                )
                .await
                {
                    ControlAction::Reply(status, text) => {
                        if reloaded {
                            starts.after_reload(manager);
                        }
                        if status == Status::Ok {
                            rlog!("control: {} ok", request.describe());
                        } else {
                            rlog!("control: {} -> {status:?}", request.describe());
                        }
                        // The client may have hung up; that is not rinit's problem.
                        let _ = reply.send((status, text));
                    }
                    ControlAction::Start(mut job) => {
                        // The reply waits until the unit is actually up: a
                        // oneshot has exited, a forking daemon has been located,
                        // and a notify service has said READY=1.
                        job.reply = Some(reply);
                        rlog!("control: {} accepted", request.describe());
                        starts.controls.push(job);
                    }
                }
                if requested.is_some() {
                    break;
                }
            }
            _ = tokio::time::sleep(wait) => {}
            // The idle half of the orphan reaping rule: even with no signal and
            // no deadline due, wake often enough to drain a child that was
            // reparented to PID 1 without a fresh SIGCHLD. The actual drain is
            // at the top of the loop, so this arm has no body.
            _ = tokio::time::sleep(HOUSEKEEPING_POLL) => {}
            Some(unit) = activation.recv() => {
                woke_socket = Some(unit);
            }
        }

        if let Some(unit) = woke_socket {
            // A connection is the request: start the service if it is not already
            // up, and either way the clock on being idle starts again.
            manager.note_connection(&unit);
            let cooling_down = socket_activations
                .get(&unit)
                .is_some_and(|at| at.elapsed() < SOCKET_ACTIVATION_COOLDOWN);
            if !manager.is_running(&unit)
                && !starts.flows.contains_key(&unit)
                && !manager.is_failed(&unit)
                && !cooling_down
            {
                // Register the start in the Job queue as well as spawning it:
                // the queue is what `shutdown_order()` reads, so a
                // socket-activated service must have a `Started` target or
                // shutdown would leave it running while filesystems unmount.
                let effect = manager.request_start(&unit, RequestSource::Dependency);
                match effect {
                    Some(effect) => match starts.begin_effect(manager, &unit, effect) {
                        Ok(()) => {
                            socket_activations.insert(unit.clone(), Instant::now());
                            rlog!("{unit}: started by a connection");
                        }
                        Err(e) => rlog!("error: cannot start {unit} for its connection: {e}"),
                    },
                    None => rlog!("{unit}: start queued behind its stop"),
                }
            }
        }
    }

    // A start that has not reached `ExecStart=` yet has no unit process for the
    // stop path to signal: kill its `ExecStartPre=`/`Post=` command here, and
    // release any control client still waiting for the reply.
    starts.shutdown();
    if fast {
        manager.compress_stop_timeouts(FAST_STOP_SECS);
    }
    for line in manager.summary() {
        rlog!("shutdown: {line}");
    }
    // Shutdown is a `ShutdownStop` request for everything the queue still
    // wants running. Going through the queue rather than stopping each unit by
    // hand is what gives shutdown its precedence (plan §1.3(11)): its source
    // outranks a dependency start, so a unit cannot be pulled back up while the
    // system is going down.
    for unit in manager.shutdown_order() {
        let effect = manager.request_stop(&unit, RequestSource::ShutdownStop);
        if let Err(e) =
            apply_effect(manager, supervised.notify, supervised.logs, &unit, effect).await
        {
            rlog!("warning: stopping {unit} failed: {e}");
        }
    }
    supervised.notify.disarm_all();
    let budget = fast.then_some(FAST_SHUTDOWN_BUDGET);
    stop_and_reap(manager, budget, watchdog).await;
    action
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
async fn stop_and_reap(
    manager: &mut UnitManager,
    budget: Option<std::time::Duration>,
    watchdog: &mut Watchdog,
) {
    let give_up = budget.map(crate::deadline::after);
    loop {
        // The shutdown wait is bounded by each unit's `TimeoutStopSec=`, but
        // that can still be tens of seconds. A watchdog armed for a shorter
        // period must not reset a machine that is shutting down correctly.
        watchdog.feed_if_due();
        for outcome in reap_into_manager(manager).outcomes {
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
    let mut env: Vec<String> = match config {
        Some(config) if config.service_kind == ServiceKind::Notify => {
            notify.arm(&config)?;
            notify.env_for(unit).into_iter().collect()
        }
        _ => Vec::new(),
    };

    // Plan §7.9: a unit rinit holds a socket for gets it on fd 3, and says so in
    // the environment - the contract a service reads to know it was activated
    // rather than started by hand.
    let listen_fd = manager.listener_fd(unit);
    if listen_fd.is_some() {
        env.push(crate::activation::LISTEN_FDS_ENV.to_string());
    }

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
    let spawned = manager.spawn_unit_with_output(
        unit,
        &env,
        output.as_ref().map(|fd| fd.as_raw_fd()),
        listen_fd,
    );
    // Close rinit's copy of the write end: as long as it is open the reader
    // never sees end-of-file, and the pump task would live forever.
    drop(output);
    spawned?;
    Ok(manager.get(unit).is_some_and(|rt| rt.pid.is_some()))
}

/// How often a start flow is polled while it is waiting.
///
/// A start can wait on a socket path, a conflicting unit, an `ExecStartPre=`
/// process or a service's readiness. Polling at this interval keeps every one of
/// those responsive without turning the supervisor loop into a busy spin.
const START_POLL: Duration = Duration::from_millis(20);

/// How long after a socket-activated start the same still-readable socket may
/// trigger another one.
///
/// The watcher is level-triggered: a connection the service did not accept keeps
/// the listener readable, so without a floor a failed or short-lived start would
/// be retried every `WATCH_FLOOR` (10 ms). A failed unit is not retried at all;
/// this covers the successful-but-already-exited case.
const SOCKET_ACTIVATION_COOLDOWN: Duration = Duration::from_secs(1);

/// When a flow finishes after the main process is spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartCompletion {
    /// Return as soon as `ExecStart=` is running. Boot uses this so a whole
    /// level is still spawned before its `oneshot` units are waited for.
    AtSpawn,
    /// Run `ExecStartPost=` and, for `oneshot`/`forking`, wait until the unit
    /// has actually finished starting. The supervisor uses this.
    AtReady,
}

/// One in-flight start, advanced one non-blocking step at a time.
///
/// The supervisor must stay responsive while a service starts: a slow
/// `ExecStartPre=`, a `NeedsSocket=` provider or a conflicting unit can take
/// seconds, and blocking the loop for that long would also stop the watchdog
/// feed, `rctl` replies and signal handling. Everything that used to be an
/// `await` in `start_unit` is a stage here; [`StartFlow::advance`] performs at
/// most the work that is ready and returns.
struct StartFlow {
    unit: UnitName,
    completion: StartCompletion,
    stage: StartStage,
    spawned: bool,
}

enum StartStage {
    /// Wait for every path a `NeedsSocket=` named (plan §7.8).
    Sockets {
        paths: Vec<String>,
        deadline: Instant,
    },
    /// Stop every conflicting unit and wait for it to go (plan §1.1).
    Conflicts {
        waits: Option<Vec<ConflictWait>>,
    },
    /// Run `ExecStartPre=` one command at a time.
    Pre {
        commands: Vec<rinit_core::ExecCommand>,
        next: usize,
        running: Option<AuxRun>,
    },
    /// Spawn `ExecStart=`.
    Spawn,
    /// Wait until a `oneshot`/`forking` unit has finished its start.
    WaitReady,
    /// Run `ExecStartPost=` one command at a time.
    Post {
        commands: Vec<rinit_core::ExecCommand>,
        next: usize,
        running: Option<AuxRun>,
    },
    Done,
}

struct ConflictWait {
    unit: UnitName,
    deadline: Instant,
    escalated: bool,
    done: bool,
}

struct AuxRun {
    pid: libc::pid_t,
    deadline: Instant,
}

impl StartFlow {
    /// Snapshot the start path for `unit`.
    ///
    /// `adopt_pending_config` runs here, before any stage reads the definition,
    /// so a reload that deferred a change to this start cannot make
    /// `ExecStartPre=` see one definition and `ExecStart=` another.
    fn new(
        manager: &mut UnitManager,
        unit: &UnitName,
        completion: StartCompletion,
    ) -> Result<Self, String> {
        manager.adopt_pending_config(unit);
        let Some(rt) = manager.get(unit) else {
            return Err(format!("{unit} is not registered"));
        };
        let paths = rt.config.needs_socket.clone();
        let stage = if paths.is_empty() {
            StartStage::Conflicts { waits: None }
        } else {
            StartStage::Sockets {
                paths,
                deadline: start_deadline(rt.config.timeout_start_sec),
            }
        };
        Ok(StartFlow {
            unit: unit.clone(),
            completion,
            stage,
            spawned: false,
        })
    }

    /// The auxiliary command this flow is waiting for, if any.
    fn running_aux_pid(&self) -> Option<libc::pid_t> {
        match &self.stage {
            StartStage::Pre {
                running: Some(run), ..
            }
            | StartStage::Post {
                running: Some(run), ..
            } => Some(run.pid),
            _ => None,
        }
    }

    /// Whether `pid` is one this flow is still waiting for.
    fn wants_pid(&self, pid: libc::pid_t) -> bool {
        self.running_aux_pid() == Some(pid)
    }

    /// Conflicting units this flow is still waiting to exit.
    ///
    /// The supervisor's global reaper clears these; the boot driver has no
    /// reaper of its own, so it polls exactly these pids and reports their
    /// exits to the manager.
    fn waiting_conflict_pids(&self, manager: &UnitManager) -> Vec<libc::pid_t> {
        match &self.stage {
            StartStage::Conflicts { waits: Some(waits) } => waits
                .iter()
                .filter(|wait| !wait.done)
                .filter_map(|wait| manager.get(&wait.unit).and_then(|rt| rt.pid))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Advance to the next thing that is ready now.
    ///
    /// `None` means "still waiting"; `Some(Ok(spawned))` is a completed start
    /// and `Some(Err(reason))` a failed one. A failed flow has already recorded
    /// the failure with [`fail_unit`].
    fn advance(
        &mut self,
        manager: &mut UnitManager,
        notify: &mut NotifyHub,
        logs: &ServiceLogs,
        aux: &mut BTreeMap<libc::pid_t, ExitOutcome>,
    ) -> Option<Result<bool, String>> {
        // A stop can arrive between any two steps now that the start path no
        // longer owns the loop. Before the spawn, the stop target is enough;
        // after it, the unit's own state says whether a stop has taken over.
        if manager.target_of(&self.unit) == Some(JobTarget::Stopped) {
            return Some(Err(format!("{} start cancelled by a stop", self.unit)));
        }
        if self.spawned
            && matches!(
                manager.get(&self.unit).map(|rt| rt.state),
                Some(UnitState::Deactivating)
            )
        {
            return Some(Err(format!("{} start cancelled by a stop", self.unit)));
        }

        loop {
            let stage = std::mem::replace(&mut self.stage, StartStage::Done);
            match stage {
                StartStage::Sockets { paths, deadline } => {
                    if let Some(missing) = paths
                        .iter()
                        .find(|path| !std::path::Path::new(path).exists())
                    {
                        if Instant::now() < deadline {
                            self.stage = StartStage::Sockets { paths, deadline };
                            return None;
                        }
                        return Some(self.failed(
                            manager,
                            format!(
                                "{}: NeedsSocket={missing} did not appear in time",
                                self.unit
                            ),
                        ));
                    }
                    self.stage = StartStage::Conflicts { waits: None };
                }
                StartStage::Conflicts { waits } => {
                    let mut waits = match waits {
                        Some(waits) => waits,
                        None => {
                            let mut waits = Vec::new();
                            for other in manager.stop_conflicts(&self.unit) {
                                let secs = manager
                                    .get(&other)
                                    .map_or(1, |rt| rt.config.timeout_stop_sec.max(1));
                                rlog!("stopping {other}: it conflicts with {}", self.unit);
                                waits.push(ConflictWait {
                                    unit: other,
                                    deadline: crate::deadline::after_secs(secs),
                                    escalated: false,
                                    done: false,
                                });
                            }
                            waits
                        }
                    };

                    let now = Instant::now();
                    for wait in &mut waits {
                        if wait.done {
                            continue;
                        }
                        let Some(pid) = manager.get(&wait.unit).and_then(|rt| rt.pid) else {
                            if !wait.done {
                                rlog!("{} stopped; {} may start", wait.unit, self.unit);
                                wait.done = true;
                            }
                            continue;
                        };
                        if now < wait.deadline {
                            continue;
                        }
                        if !wait.escalated {
                            rlog!(
                                "warning: {} did not stop for {}; killing it",
                                wait.unit,
                                self.unit
                            );
                            let _ = process::kill_process_group(pid, libc::SIGKILL);
                            wait.escalated = true;
                            wait.deadline = crate::deadline::after_secs(5);
                        } else {
                            rlog!(
                                "warning: {} is still running; starting {} anyway",
                                wait.unit,
                                self.unit
                            );
                            wait.done = true;
                        }
                    }
                    if waits.iter().any(|wait| !wait.done) {
                        self.stage = StartStage::Conflicts { waits: Some(waits) };
                        return None;
                    }
                    let pre = manager
                        .get(&self.unit)
                        .map(|rt| rt.config.exec_start_pre.clone())
                        .unwrap_or_default();
                    self.stage = StartStage::Pre {
                        commands: pre,
                        next: 0,
                        running: None,
                    };
                }
                StartStage::Pre {
                    commands,
                    mut next,
                    running,
                } => {
                    if let Some(run) = running {
                        match aux.remove(&run.pid) {
                            Some(ExitOutcome::Success) => {
                                next += 1;
                                self.stage = StartStage::Pre {
                                    commands,
                                    next,
                                    running: None,
                                };
                                continue;
                            }
                            Some(outcome) => {
                                return Some(self.failed(
                                    manager,
                                    format!(
                                        "{}: ExecStartPre '{}' failed ({})",
                                        self.unit,
                                        command_text(&commands, next),
                                        describe_exit(outcome)
                                    ),
                                ));
                            }
                            None if Instant::now() >= run.deadline => {
                                let _ = process::kill_process_group(run.pid, libc::SIGKILL);
                                return Some(self.failed(
                                    manager,
                                    format!(
                                        "{}: ExecStartPre '{}' timed out",
                                        self.unit,
                                        command_text(&commands, next)
                                    ),
                                ));
                            }
                            None => {
                                self.stage = StartStage::Pre {
                                    commands,
                                    next,
                                    running: Some(run),
                                };
                                return None;
                            }
                        }
                    }

                    let Some(cmd) = commands.get(next) else {
                        self.stage = StartStage::Spawn;
                        continue;
                    };
                    match plan_aux_command(manager, &self.unit, cmd) {
                        Ok(plan) => match spawn_aux_command(&self.unit, &plan) {
                            Ok(pid) => {
                                self.stage = StartStage::Pre {
                                    commands,
                                    next,
                                    running: Some(AuxRun {
                                        pid,
                                        deadline: start_deadline(plan.timeout_sec),
                                    }),
                                };
                                return None;
                            }
                            Err(reason) => return Some(self.failed(manager, reason)),
                        },
                        Err(reason) => return Some(self.failed(manager, reason)),
                    }
                }
                StartStage::Spawn => match spawn_unit(manager, notify, logs, &self.unit) {
                    Ok(spawned) => {
                        self.spawned = spawned;
                        if self.completion == StartCompletion::AtSpawn {
                            self.stage = StartStage::Done;
                            return Some(Ok(spawned));
                        }
                        let kind = manager
                            .get(&self.unit)
                            .map_or(ServiceKind::Simple, |rt| rt.config.service_kind);
                        self.stage = if matches!(kind, ServiceKind::Oneshot | ServiceKind::Forking)
                        {
                            StartStage::WaitReady
                        } else {
                            let post = manager
                                .get(&self.unit)
                                .map(|rt| rt.config.exec_start_post.clone())
                                .unwrap_or_default();
                            StartStage::Post {
                                commands: post,
                                next: 0,
                                running: None,
                            }
                        };
                    }
                    Err(e) => {
                        return Some(self.failed(
                            manager,
                            format!(
                                "{}: cannot start '{}': {e}",
                                self.unit,
                                exec_start_of(manager, &self.unit)
                            ),
                        ));
                    }
                },
                StartStage::WaitReady => {
                    let Some(rt) = manager.get(&self.unit) else {
                        return Some(Err(format!("{} is no longer registered", self.unit)));
                    };
                    match rt.state {
                        UnitState::Active | UnitState::ActiveExited => {
                            let post = rt.config.exec_start_post.clone();
                            self.stage = StartStage::Post {
                                commands: post,
                                next: 0,
                                running: None,
                            };
                        }
                        UnitState::Starting => {
                            self.stage = StartStage::WaitReady;
                            return None;
                        }
                        UnitState::Failed(_)
                        | UnitState::RestartWait
                        | UnitState::Inactive
                        | UnitState::Deactivating => {
                            return Some(Err(format!("{} failed to start", self.unit)));
                        }
                    }
                }
                StartStage::Post {
                    commands,
                    mut next,
                    running,
                } => {
                    if let Some(run) = running {
                        match aux.remove(&run.pid) {
                            Some(ExitOutcome::Success) => {}
                            Some(outcome) => rlog!(
                                "warning: {}: ExecStartPost '{}' failed ({})",
                                self.unit,
                                command_text(&commands, next),
                                describe_exit(outcome)
                            ),
                            None if Instant::now() >= run.deadline => {
                                let _ = process::kill_process_group(run.pid, libc::SIGKILL);
                                rlog!(
                                    "warning: {}: ExecStartPost '{}' timed out",
                                    self.unit,
                                    command_text(&commands, next)
                                );
                            }
                            None => {
                                self.stage = StartStage::Post {
                                    commands,
                                    next,
                                    running: Some(run),
                                };
                                return None;
                            }
                        }
                        next += 1;
                    }

                    let Some(cmd) = commands.get(next) else {
                        self.stage = StartStage::Done;
                        return Some(Ok(self.spawned));
                    };
                    match plan_aux_command(manager, &self.unit, cmd) {
                        Ok(plan) => match spawn_aux_command(&self.unit, &plan) {
                            Ok(pid) => {
                                self.stage = StartStage::Post {
                                    commands,
                                    next,
                                    running: Some(AuxRun {
                                        pid,
                                        deadline: start_deadline(plan.timeout_sec),
                                    }),
                                };
                                return None;
                            }
                            Err(reason) => {
                                rlog!("warning: {reason}");
                                next += 1;
                                self.stage = StartStage::Post {
                                    commands,
                                    next,
                                    running: None,
                                };
                            }
                        },
                        Err(reason) => {
                            rlog!("warning: {reason}");
                            next += 1;
                            self.stage = StartStage::Post {
                                commands,
                                next,
                                running: None,
                            };
                        }
                    }
                }
                StartStage::Done => return Some(Ok(self.spawned)),
            }
        }
    }

    fn failed(&mut self, manager: &mut UnitManager, reason: String) -> Result<bool, String> {
        rlog!("error: {reason}");
        fail_unit(manager, &self.unit);
        Err(reason)
    }
}

/// A deadline for `secs`, or the largest one rinit can represent for `0`.
fn start_deadline(secs: u64) -> Instant {
    if secs == 0 {
        crate::deadline::after(crate::deadline::MAX_TIMEOUT)
    } else {
        crate::deadline::after_secs(secs)
    }
}

/// Turn one `waitpid` result into the manager's exit vocabulary.
fn child_outcome(exit: rinit_runtime::syscall::wait::ChildExit) -> ExitOutcome {
    match exit {
        rinit_runtime::syscall::wait::ChildExit::Exited { code: 0, .. } => ExitOutcome::Success,
        rinit_runtime::syscall::wait::ChildExit::Exited { code, .. } => ExitOutcome::Failure(code),
        rinit_runtime::syscall::wait::ChildExit::Signaled { signal, .. } => {
            ExitOutcome::Signalled(signal)
        }
    }
}

/// Describe an auxiliary command's exit for its log line.
fn describe_exit(outcome: ExitOutcome) -> String {
    match outcome {
        ExitOutcome::Success => "exit code 0".to_string(),
        ExitOutcome::Failure(code) => format!("exit code {code}"),
        ExitOutcome::Signalled(signal) => format!("killed by signal {signal}"),
        ExitOutcome::TimedOut => "timed out".to_string(),
    }
}

/// The command line for a log message, or an empty string for a vanished step.
fn command_text(commands: &[rinit_core::ExecCommand], index: usize) -> String {
    commands
        .get(index)
        .map(|cmd| cmd.argv().join(" "))
        .unwrap_or_default()
}

/// One `ExecStartPre=`/`ExecStartPost=` command, resolved against the unit's
/// current definition.
struct AuxCommandPlan {
    argv: Vec<String>,
    env: Vec<String>,
    cwd: String,
    sandbox: rinit_core::unit::sandbox::Sandbox,
    timeout_sec: u64,
}

fn plan_aux_command(
    manager: &UnitManager,
    unit: &UnitName,
    cmd: &rinit_core::ExecCommand,
) -> Result<AuxCommandPlan, String> {
    let Some(rt) = manager.get(unit) else {
        return Err(format!("{unit} is not registered"));
    };
    let argv = cmd.argv().to_vec();
    let sandbox = crate::sandbox::resolve(&rt.config)
        .map_err(|e| format!("{unit}: cannot start '{}': {e}", argv.join(" ")))?;
    Ok(AuxCommandPlan {
        argv,
        env: build_service_env(&rt.config, &[]),
        cwd: rt.config.working_directory.clone(),
        sandbox,
        timeout_sec: rt.config.timeout_start_sec,
    })
}

fn spawn_aux_command(unit: &UnitName, plan: &AuxCommandPlan) -> Result<libc::pid_t, String> {
    // As before the state machine: preparation is not the service. It runs
    // outside the unit's cgroup and without its `TTYPath=`.
    spawn_service_in_group(
        &plan.argv,
        &plan.env,
        Some(&plan.cwd),
        None,
        &plan.sandbox,
        crate::service::ChildPlacement::default(),
    )
    .map_err(|e| format!("{unit}: cannot start '{}': {e}", plan.argv.join(" ")))
}

/// Run one auxiliary command to completion for the boot path.
///
/// Boot ordering is synchronous by design, so the boot half of the start path
/// keeps a direct wait while the supervisor uses [`StartFlow`].
async fn run_aux_command(
    manager: &UnitManager,
    unit: &UnitName,
    cmd: &rinit_core::ExecCommand,
) -> Option<i32> {
    let plan = match plan_aux_command(manager, unit, cmd) {
        Ok(plan) => plan,
        Err(reason) => {
            rlog!("error: {reason}");
            return None;
        }
    };
    let pid = match spawn_aux_command(unit, &plan) {
        Ok(pid) => pid,
        Err(reason) => {
            rlog!("error: {reason}");
            return None;
        }
    };
    let timeout = if plan.timeout_sec == 0 {
        crate::deadline::MAX_TIMEOUT
    } else {
        Duration::from_secs(plan.timeout_sec)
    };
    match readiness::wait_for_exit(pid, timeout).await {
        Ok(code) => code,
        Err(()) => {
            // Timed out: make sure it is gone before carrying on.
            let _ = process::kill_process_group(pid, libc::SIGKILL);
            let _ = readiness::wait_for_exit(pid, Duration::from_secs(5)).await;
            rlog!("error: {unit}: '{}' timed out", plan.argv.join(" "));
            None
        }
    }
}

/// Drive a [`StartFlow`] to completion for the boot path.
///
/// Boot has no supervisor loop yet, so it waits directly. The flow still owns
/// the rules; only the wait is synchronous here.
async fn start_unit(
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    logs: &ServiceLogs,
    unit: &UnitName,
) -> Result<bool, RuntimeError> {
    let mut flow = match StartFlow::new(manager, unit, StartCompletion::AtSpawn) {
        Ok(flow) => flow,
        Err(reason) => {
            rlog!("error: {reason}");
            return Ok(false);
        }
    };
    let mut aux = BTreeMap::new();
    loop {
        if let Some(pid) = flow.running_aux_pid()
            && let Ok(Some(exit)) = rinit_runtime::syscall::wait::poll_child(pid)
        {
            aux.insert(pid, child_outcome(exit));
        }
        // Boot has no global reaper, so the conflicting units this flow stopped
        // are polled here. Their exits still go through the manager, which owns
        // the state transition and the cgroup cleanup.
        for pid in flow.waiting_conflict_pids(manager) {
            if let Ok(Some(exit)) = rinit_runtime::syscall::wait::poll_child(pid)
                && let Some(outcome) = manager.on_child_exit(pid, child_outcome(exit))
            {
                log_outcome(&outcome);
            }
        }
        match flow.advance(manager, notify, logs, &mut aux) {
            Some(Ok(spawned)) => return Ok(spawned),
            Some(Err(_)) => return Ok(false),
            None => tokio::time::sleep(START_POLL).await,
        }
    }
}

/// Every start the supervisor is driving, one step per loop iteration.
///
/// The map is keyed by unit so a restart due, a queued start and a connection
/// that all arrive together cannot spawn the same unit twice.
#[derive(Default)]
pub(crate) struct StartCoordinator {
    flows: BTreeMap<UnitName, StartFlow>,
    controls: Vec<ControlStart>,
    /// Exits of auxiliary commands, put here by the global reaper.
    aux_exits: BTreeMap<libc::pid_t, ExitOutcome>,
}

impl StartCoordinator {
    /// Add `unit` to the flows if it is not already being started.
    fn begin(
        &mut self,
        manager: &mut UnitManager,
        unit: &UnitName,
        completion: StartCompletion,
    ) -> Result<(), String> {
        if self.flows.contains_key(unit) {
            return Ok(());
        }
        let flow = StartFlow::new(manager, unit, completion)?;
        self.flows.insert(unit.clone(), flow);
        Ok(())
    }

    /// Apply a start effect by creating the flow it asks for.
    pub(crate) fn begin_effect(
        &mut self,
        manager: &mut UnitManager,
        unit: &UnitName,
        effect: Effect,
    ) -> Result<(), String> {
        apply_start_effect(manager, &mut self.flows, unit, effect)
    }

    /// Reap children, keeping exits an in-flight auxiliary command is waiting
    /// for. The unit outcomes are returned for the caller's usual logging.
    fn reap(&mut self, manager: &mut UnitManager) -> Vec<Outcome> {
        let report = reap_into_manager(manager);
        for (pid, outcome) in report.unclaimed {
            if self.flows.values().any(|flow| flow.wants_pid(pid)) {
                self.aux_exits.insert(pid, outcome);
            }
        }
        report.outcomes
    }

    /// Advance every start and control job once.
    fn advance(&mut self, manager: &mut UnitManager, notify: &mut NotifyHub, logs: &ServiceLogs) {
        let units: Vec<UnitName> = self.flows.keys().cloned().collect();
        for unit in units {
            let done = match self.flows.get_mut(&unit) {
                Some(flow) => flow.advance(manager, notify, logs, &mut self.aux_exits),
                None => None,
            };
            if done.is_some() {
                self.flows.remove(&unit);
            }
        }

        let mut index = 0;
        while index < self.controls.len() {
            // The control server gives a client 60 s; if it has gone away, the
            // start can keep running but its reply no longer needs a slot.
            if self
                .controls
                .get(index)
                .and_then(|job| job.reply.as_ref())
                .is_some_and(oneshot::Sender::is_closed)
            {
                self.controls.remove(index);
                continue;
            }
            let done = match self.controls.get_mut(index) {
                Some(job) => {
                    job.advance(manager, &mut self.flows, notify, logs, &mut self.aux_exits)
                }
                None => break,
            };
            if let Some((status, text)) = done {
                let mut job = self.controls.remove(index);
                if let Some(reply) = job.reply.take() {
                    let _ = reply.send((status, text.clone()));
                }
                if status == Status::Ok {
                    rlog!("control: start {} ok", job.target);
                } else {
                    rlog!("control: start {} -> {status:?}", job.target);
                }
            } else {
                index += 1;
            }
        }
    }

    /// Abandon an in-flight start because the unit was asked to stop.
    ///
    /// Killing the auxiliary command is the part the normal stop path cannot do:
    /// it has no pid for an `ExecStartPre=` process, so without this the command
    /// would outlive the stop and could still create the state the start was
    /// preparing.
    fn cancel(&mut self, unit: &UnitName) {
        let Some(flow) = self.flows.remove(unit) else {
            return;
        };
        if let Some(pid) = flow.running_aux_pid() {
            let _ = process::kill_process_group(pid, libc::SIGKILL);
            self.aux_exits.remove(&pid);
        }
    }

    /// Drop work whose unit a reload removed.
    ///
    /// A start that has not spawned yet has no process for the reload's stop
    /// pass to signal, and a deferred control client waiting for a unit that no
    /// longer exists needs an answer rather than a timeout.
    fn after_reload(&mut self, manager: &mut UnitManager) {
        let units: Vec<UnitName> = self.flows.keys().cloned().collect();
        for unit in units {
            if !manager.exists(&unit) {
                self.cancel(&unit);
            }
        }

        let mut index = 0;
        while index < self.controls.len() {
            let target = match self.controls.get(index) {
                Some(job) => job.target.clone(),
                None => break,
            };
            if manager.exists(&target) {
                index += 1;
                continue;
            }
            let mut job = self.controls.remove(index);
            if let Some(reply) = job.reply.take() {
                let _ = reply.send((Status::NoSuchUnit, format!("no such unit: {target}")));
            }
        }
    }

    /// How long the supervisor may sleep with starts in flight.
    fn next_wait(&self) -> Duration {
        if self.flows.is_empty() && self.controls.is_empty() {
            Duration::from_secs(3600)
        } else {
            START_POLL
        }
    }

    /// Abandon every in-flight start on shutdown, killing any auxiliary command
    /// that is still running so it is not orphaned past the service tree.
    fn shutdown(&mut self) {
        for flow in self.flows.values() {
            if let Some(pid) = flow.running_aux_pid() {
                let _ = process::kill_process_group(pid, libc::SIGKILL);
                self.aux_exits.remove(&pid);
            }
        }
        for job in &mut self.controls {
            if let Some(reply) = job.reply.take() {
                let _ = reply.send((Status::Error, "shutting down".to_string()));
            }
        }
        self.flows.clear();
        self.controls.clear();
        self.aux_exits.clear();
    }
}

/// A deferred `rctl start`: dependencies are brought up in boot order, each
/// waited for before the next starts, and the reply is sent when the target is
/// actually up (or failed).
struct ControlStart {
    target: UnitName,
    order: Vec<UnitName>,
    required: BTreeSet<UnitName>,
    index: usize,
    waiting: Option<UnitName>,
    reply: Option<oneshot::Sender<(Status, String)>>,
    target_was_active: bool,
}

impl ControlStart {
    fn new(manager: &UnitManager, unit: &UnitName) -> Result<Self, String> {
        let mut order = manager.start_order(unit)?;
        order.push(unit.clone());
        let required = manager.required_closure(unit).into_iter().collect();
        let target_was_active = manager
            .get(unit)
            .is_some_and(|rt| matches!(rt.state, UnitState::Active | UnitState::ActiveExited));
        Ok(ControlStart {
            target: unit.clone(),
            order,
            required,
            index: 0,
            waiting: None,
            reply: None,
            target_was_active,
        })
    }

    fn advance(
        &mut self,
        manager: &mut UnitManager,
        flows: &mut BTreeMap<UnitName, StartFlow>,
        notify: &mut NotifyHub,
        logs: &ServiceLogs,
        aux: &mut BTreeMap<libc::pid_t, ExitOutcome>,
    ) -> Option<(Status, String)> {
        loop {
            if let Some(waiting) = self.waiting.clone() {
                if flows.contains_key(&waiting) {
                    return None;
                }
                let Some(rt) = manager.get(&waiting) else {
                    return Some((Status::NoSuchUnit, format!("no such unit: {waiting}")));
                };
                match rt.state {
                    UnitState::Active | UnitState::ActiveExited => {
                        self.waiting = None;
                    }
                    UnitState::Starting => return None,
                    UnitState::Failed(_) => {
                        if waiting == self.target || self.required.contains(&waiting) {
                            return Some((Status::Error, failure_text(&waiting, &self.target)));
                        }
                        rlog!(
                            "warning: {waiting} failed, but {} does not require it",
                            self.target
                        );
                        self.waiting = None;
                    }
                    UnitState::RestartWait | UnitState::Inactive | UnitState::Deactivating => {
                        if waiting == self.target || self.required.contains(&waiting) {
                            return Some((Status::Error, failure_text(&waiting, &self.target)));
                        }
                        self.waiting = None;
                    }
                }
                continue;
            }

            let Some(unit) = self.order.get(self.index).cloned() else {
                let text = if self.target_was_active {
                    format!("{} is already active", self.target)
                } else {
                    format!("started {}", self.target)
                };
                return Some((Status::Ok, text));
            };
            self.index += 1;

            if manager
                .get(&unit)
                .is_some_and(|rt| matches!(rt.state, UnitState::Active | UnitState::ActiveExited))
            {
                continue;
            }
            if flows.contains_key(&unit) {
                self.waiting = Some(unit);
                continue;
            }

            let source = if unit == self.target {
                RequestSource::UserStart
            } else {
                RequestSource::Dependency
            };
            let effect = manager.request_start(&unit, source);
            match effect {
                Some(Effect::None) => {
                    self.waiting = Some(unit);
                }
                None => {
                    if unit == self.target {
                        return Some((
                            Status::Ok,
                            format!("{unit} is stopping; it will start afterwards"),
                        ));
                    }
                    return Some((
                        Status::Error,
                        format!("dependency {unit} is still stopping"),
                    ));
                }
                Some(effect) => {
                    if let Err(reason) = apply_start_effect(manager, flows, &unit, effect) {
                        let what = if unit == self.target {
                            format!("cannot start {unit}: {reason}")
                        } else {
                            format!("cannot start dependency {unit}: {reason}")
                        };
                        return Some((Status::Error, what));
                    }
                    // Advance the freshly created flow once, right here. A
                    // `simple` unit with no `ExecStartPre=` then spawns and
                    // finishes inside this loop, so a `rctl start` does not pay
                    // a 20 ms polling tick just to see the flow it just made.
                    if let Some(flow) = flows.get_mut(&unit)
                        && flow.advance(manager, notify, logs, aux).is_some()
                    {
                        flows.remove(&unit);
                    }
                    self.waiting = Some(unit);
                }
            }
        }
    }
}

fn failure_text(failed: &UnitName, target: &UnitName) -> String {
    if failed == target {
        format!("{target} failed to start")
    } else {
        format!("required dependency {failed} failed to start")
    }
}

/// Turn a start effect into a flow. The stop variants cannot come from a start
/// request; treating them as an error keeps an unexpected effect visible.
fn apply_start_effect(
    manager: &mut UnitManager,
    flows: &mut BTreeMap<UnitName, StartFlow>,
    unit: &UnitName,
    effect: Effect,
) -> Result<(), String> {
    match effect {
        Effect::SpawnProcess | Effect::CancelBackoffAndStart => {
            if !flows.contains_key(unit) {
                let flow = StartFlow::new(manager, unit, StartCompletion::AtReady)?;
                flows.insert(unit.clone(), flow);
            }
            Ok(())
        }
        Effect::ClearStartLimitAndStart => {
            manager.clear_start_limit(unit);
            if !flows.contains_key(unit) {
                let flow = StartFlow::new(manager, unit, StartCompletion::AtReady)?;
                flows.insert(unit.clone(), flow);
            }
            Ok(())
        }
        Effect::None => Ok(()),
        Effect::SendStopSignal | Effect::CancelBackoffAndStop => {
            manager.stop_unit(unit);
            Err(format!("{unit} was asked to stop while starting"))
        }
        Effect::ScheduleBackoff { .. } => Err(format!("unexpected start effect for {unit}")),
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
        // `JobQueue::reconcile` does not produce one: a backoff is armed by the
        // reaper. Reaching one here would be a bug worth seeing.
        Some(Effect::ScheduleBackoff { .. }) => {
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
enum ControlAction {
    /// The request is answered now.
    Reply(Status, String),
    /// A start is in flight; the supervisor owns its reply so the main loop can
    /// keep running while the unit comes up.
    Start(ControlStart),
}

async fn handle_control(
    cfg: &Config,
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    logs: &ServiceLogs,
    activation: &mut Activated,
    starts: &mut StartCoordinator,
    request: &Request,
) -> ControlAction {
    match request {
        Request::List | Request::Status { unit: None, .. } => {
            ControlAction::Reply(Status::Ok, manager.summary().join("\n"))
        }
        Request::ListFormatted { format } => {
            ControlAction::Reply(Status::Ok, render_list(manager, *format))
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
                ControlAction::Reply(Status::Ok, text)
            }
            None => ControlAction::Reply(Status::NoSuchUnit, format!("no such unit: {unit}")),
        },
        Request::StatusFormatted {
            unit: Some(unit),
            tail,
            format,
        } => match describe_unit_formatted(manager, unit, *format) {
            Some(mut text) => {
                if *tail > 0 {
                    let logs_text = render_logs(logs, Some(unit), *tail);
                    if !logs_text.is_empty() {
                        if *format == StatusFormat::Json {
                            // The JSON reply is one object; a textual log block
                            // would make it invalid. Drop the object's closing
                            // brace, add the field, and close it again.
                            if text.ends_with('}') {
                                text.pop();
                            }
                            text.push_str(",\"log\":");
                            text.push_str(&json_string(&logs_text));
                            text.push('}');
                        } else {
                            text.push_str("log:\n");
                            text.push_str(&logs_text);
                            text.push('\n');
                        }
                    }
                }
                ControlAction::Reply(Status::Ok, text)
            }
            None => ControlAction::Reply(Status::NoSuchUnit, format!("no such unit: {unit}")),
        },
        Request::StatusFormatted {
            unit: None, format, ..
        } => {
            let lines = manager.summary();
            let text = if *format == StatusFormat::Json {
                let items: Vec<String> = lines.iter().map(|line| json_string(line)).collect();
                format!("[{}]", items.join(","))
            } else {
                lines.join("\n")
            };
            ControlAction::Reply(Status::Ok, text)
        }
        Request::Start(unit) => control_start(manager, unit),
        Request::Stop(unit) => {
            let (status, text) = control_stop(manager, notify, logs, unit).await;
            ControlAction::Reply(status, text)
        }
        Request::Restart(unit) => control_restart(manager, unit),
        Request::ResetFailed(unit) => {
            if !manager.exists(unit) {
                return ControlAction::Reply(Status::NoSuchUnit, format!("no such unit: {unit}"));
            }
            manager.reset_failed(unit);
            ControlAction::Reply(Status::Ok, format!("{unit}: cleared"))
        }
        Request::Log { unit, tail } => {
            ControlAction::Reply(Status::Ok, render_logs(logs, unit.as_ref(), *tail))
        }
        // Plan §7.6. The reply is the report: what was added, changed, removed
        // and deferred, so a reload is never a silent act.
        Request::Reload => {
            let (status, text) =
                control_reload(cfg, manager, notify, logs, activation, starts).await;
            ControlAction::Reply(status, text)
        }
        // The supervisor handles the side effect (it owns the shutdown path);
        // this reply only tells the client the request was understood.
        Request::Reboot => ControlAction::Reply(Status::Ok, "reboot requested".to_string()),
        Request::Poweroff => ControlAction::Reply(Status::Ok, "poweroff requested".to_string()),
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
    activation: &mut Activated,
    starts: &mut StartCoordinator,
) -> (Status, String) {
    match crate::reload::reload(cfg, manager, notify, logs, activation, starts).await {
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
fn control_start(manager: &mut UnitManager, unit: &UnitName) -> ControlAction {
    if !manager.exists(unit) {
        // Plan §7.7's boundary: a template is a name to instantiate, not a unit,
        // and saying which is which is the whole message.
        if manager.is_template(unit) {
            return ControlAction::Reply(
                Status::Error,
                format!(
                    "{unit} is a template; start an instance, e.g. {}@<name>.service",
                    unit.prefix()
                ),
            );
        }
        // An instance of a template does not exist until someone asks for it.
        if let Err(why) = manager.instantiate(unit) {
            return ControlAction::Reply(
                Status::NoSuchUnit,
                format!("no such unit: {unit} ({why})"),
            );
        }
    }
    match ControlStart::new(manager, unit) {
        Ok(job) => ControlAction::Start(job),
        Err(e) => ControlAction::Reply(
            Status::Error,
            format!("cannot resolve dependencies for {unit}: {e}"),
        ),
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
fn control_restart(manager: &mut UnitManager, unit: &UnitName) -> ControlAction {
    if !manager.exists(unit) {
        return ControlAction::Reply(Status::NoSuchUnit, format!("no such unit: {unit}"));
    }
    match manager.restart_unit(unit) {
        RestartPlan::Stopping => {
            rlog!("restarting {unit}");
            ControlAction::Reply(Status::Ok, format!("restarting {unit}"))
        }
        RestartPlan::Start => control_start(manager, unit),
        RestartPlan::NotApplicable => ControlAction::Reply(
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

/// Render `rctl list` in its selected format.
fn render_list(manager: &UnitManager, format: ListFormat) -> String {
    match format {
        // Plain is the stable compatibility path: it must not pay for the
        // per-unit resource reads that the richer views need.
        ListFormat::Plain => manager.summary().join("\n"),
        ListFormat::Short => {
            let entries = manager.list_entries();
            entries
                .iter()
                .map(|entry| {
                    format!(
                        "{}  {}  pid={}  target={}  mem={}  cpu={}",
                        entry.name,
                        entry.state,
                        entry
                            .pid
                            .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                        entry.target,
                        memory_value(entry.memory_bytes),
                        cpu_value(entry.cpu_usec)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        ListFormat::Json => {
            let rows: Vec<String> = manager.list_entries().iter().map(list_entry_json).collect();
            format!("[{}]", rows.join(","))
        }
        ListFormat::Table => render_list_table(&manager.list_entries()),
    }
}

fn render_list_table(entries: &[UnitListEntry]) -> String {
    let headers = ["UNIT", "TYPE", "STATE", "PID", "TARGET", "MEM", "CPU"];
    let rows: Vec<[String; 7]> = entries
        .iter()
        .map(|entry| {
            [
                entry.name.clone(),
                entry.kind.to_string(),
                entry.state.clone(),
                entry
                    .pid
                    .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                entry.target.to_string(),
                memory_value(entry.memory_bytes),
                cpu_value(entry.cpu_usec),
            ]
        })
        .collect();

    let mut widths = headers.map(str::len);
    for row in &rows {
        for (width, value) in widths.iter_mut().zip(row.iter()) {
            *width = (*width).max(value.chars().count());
        }
    }

    let header = headers
        .iter()
        .zip(widths.iter())
        .map(|(value, width)| format!("{value:<width$}", width = *width))
        .collect::<Vec<_>>()
        .join("  ");
    let body = rows
        .iter()
        .map(|row| {
            row.iter()
                .zip(widths.iter())
                .map(|(value, width)| format!("{value:<width$}", width = *width))
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    if body.is_empty() {
        header
    } else {
        format!("{header}\n{body}")
    }
}

fn list_entry_json(entry: &UnitListEntry) -> String {
    format!(
        "{{\"unit\":{},\"type\":{},\"state\":{},\"pid\":{},\"target\":{},\"memory_bytes\":{},\"cpu_usec\":{},\"tasks\":{}}}",
        json_string(&entry.name),
        json_string(entry.kind),
        json_string(&entry.state),
        entry
            .pid
            .map_or_else(|| "null".to_string(), |pid| pid.to_string()),
        json_string(entry.target),
        json_num(entry.memory_bytes),
        json_num(entry.cpu_usec),
        json_num(entry.tasks)
    )
}

fn json_num(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_string(), |value| value.to_string())
}

fn memory_value(bytes: Option<u64>) -> String {
    memory_text(ResourceUsage {
        memory_bytes: bytes,
        ..ResourceUsage::default()
    })
}

fn cpu_value(usec: Option<u64>) -> String {
    cpu_text(ResourceUsage {
        cpu_usec: usec,
        ..ResourceUsage::default()
    })
}

/// Render one unit in the format selected by `rctl status --format`.
fn describe_unit_formatted(
    manager: &UnitManager,
    unit: &UnitName,
    format: StatusFormat,
) -> Option<String> {
    if !manager.exists(unit) {
        return None;
    }
    let rt = manager.get(unit)?;
    let target = match manager.target_of(unit) {
        Some(JobTarget::Started) => "started",
        Some(JobTarget::Stopped) => "stopped",
        None => "none",
    };
    let usage = manager.resource_usage(unit);
    Some(match format {
        StatusFormat::Plain => render_plain(rt, target, usage),
        StatusFormat::Systemd => render_systemd(rt, target, usage),
        StatusFormat::Short => render_short(rt, target, usage),
        StatusFormat::Json => render_json(rt, target, usage),
        StatusFormat::Card => render_card(rt, target, usage),
    })
}

fn render_plain(rt: &UnitRuntime, target: &str, usage: ResourceUsage) -> String {
    let mut out = String::new();
    out.push_str(&format!("unit: {}\n", rt.config.name));
    if !rt.config.description.is_empty() {
        out.push_str(&format!("description: {}\n", rt.config.description));
    }
    out.push_str(&format!("type: {}\n", unit_type_text(rt)));
    out.push_str(&format!("state: {}\n", state_text(rt)));
    out.push_str(&format!("target: {target}\n"));
    out.push_str(&format!(
        "restarts: {}\n",
        rt.backoff.consecutive_restarts()
    ));
    out.push_str(&format!("memory: {}\n", memory_text(usage)));
    out.push_str(&format!("cpu: {}\n", cpu_text(usage)));
    if let Some(tasks) = usage.tasks {
        out.push_str(&format!("tasks: {tasks}\n"));
    }
    out.push_str(&format!("restart-policy: {}\n", restart_text(rt)));
    out.push_str(&format!("user: {}\n", user_text(rt)));
    out.push_str(&format!("exec-start: {}\n", exec_text(rt)));
    if let Some(status) = &rt.status {
        out.push_str(&format!("status: {status}\n"));
    }
    if let Some(healthy) = rt.healthy {
        out.push_str(&format!("healthy: {healthy}\n"));
    }
    if let Some(outcome) = outcome_text(rt.last_outcome) {
        out.push_str(&format!("last-exit: {outcome}\n"));
    }
    out
}

fn render_systemd(rt: &UnitRuntime, target: &str, usage: ResourceUsage) -> String {
    let name = &rt.config.name;
    let description = if rt.config.description.is_empty() {
        name.canonical()
    } else {
        rt.config.description.as_str()
    };
    let mut out = format!("● {name} - {description}\n");
    out.push_str(&format!("     Type: {}\n", unit_type_text(rt)));
    out.push_str(&format!("   Active: {}\n", state_text(rt)));
    out.push_str(&format!("   Target: {target}\n"));
    out.push_str(&format!(
        " Main PID: {}\n",
        rt.pid
            .map_or_else(|| "-".to_string(), |pid| pid.to_string())
    ));
    out.push_str(&format!("   Memory: {}\n", memory_text(usage)));
    out.push_str(&format!("      CPU: {}\n", cpu_text(usage)));
    if let Some(tasks) = usage.tasks {
        out.push_str(&format!("    Tasks: {tasks}\n"));
    }
    out.push_str(&format!(
        " Restarts: {}\n",
        rt.backoff.consecutive_restarts()
    ));
    out.push_str(&format!("  Restart: {}\n", restart_text(rt)));
    out.push_str(&format!("     User: {}\n", user_text(rt)));
    out.push_str(&format!("  Command: {}\n", exec_text(rt)));
    if let Some(status) = &rt.status {
        out.push_str(&format!("   Status: {status}\n"));
    }
    if let Some(healthy) = rt.healthy {
        out.push_str(&format!("   Health: {healthy}\n"));
    }
    if let Some(outcome) = outcome_text(rt.last_outcome) {
        out.push_str(&format!(" Last exit: {outcome}\n"));
    }
    out
}

fn render_short(rt: &UnitRuntime, target: &str, usage: ResourceUsage) -> String {
    format!(
        "{}  {}  pid={}  target={}  mem={}  cpu={}",
        rt.config.name,
        rt.state,
        rt.pid
            .map_or_else(|| "-".to_string(), |pid| pid.to_string()),
        target,
        memory_text(usage),
        cpu_text(usage)
    )
}

fn render_json(rt: &UnitRuntime, target: &str, usage: ResourceUsage) -> String {
    let pid = rt
        .pid
        .map_or_else(|| "null".to_string(), |pid| pid.to_string());
    let memory = usage
        .memory_bytes
        .map_or_else(|| "null".to_string(), |value| value.to_string());
    let cpu = usage
        .cpu_usec
        .map_or_else(|| "null".to_string(), |value| value.to_string());
    let tasks = usage
        .tasks
        .map_or_else(|| "null".to_string(), |value| value.to_string());
    format!(
        "{{\"unit\":{},\"description\":{},\"type\":{},\"state\":{},\"pid\":{},\"target\":{},\"restarts\":{},\"memory_bytes\":{},\"cpu_usec\":{},\"tasks\":{},\"restart_policy\":{},\"user\":{},\"exec_start\":{},\"status\":{},\"healthy\":{},\"last_exit\":{}}}",
        json_string(rt.config.name.canonical()),
        json_string(&rt.config.description),
        json_string(&unit_type_text(rt)),
        json_string(&state_text(rt)),
        pid,
        json_string(target),
        rt.backoff.consecutive_restarts(),
        memory,
        cpu,
        tasks,
        json_string(&restart_text(rt)),
        json_string(&user_text(rt)),
        json_string(&exec_text(rt)),
        json_opt(&rt.status),
        json_bool(rt.healthy),
        json_opt(&outcome_text(rt.last_outcome))
    )
}

fn render_card(rt: &UnitRuntime, target: &str, usage: ResourceUsage) -> String {
    let width: usize = 62;
    let name = rt.config.name.canonical();
    let description = if rt.config.description.is_empty() {
        "-".to_string()
    } else {
        rt.config.description.clone()
    };
    let mut out = String::new();
    out.push_str(&format!(
        "╭─ {} {}╮\n",
        name,
        "─".repeat(width.saturating_sub(name.len()))
    ));
    out.push_str(&format!("│ {} │\n", fit(&description, width)));
    out.push_str(&format!(
        "│ {} │\n",
        fit(&format!("● {}", state_text(rt)), width)
    ));
    out.push_str(&format!(
        "│ {} │\n",
        fit(
            &format!(
                "target: {target}   restarts: {}   tasks: {}",
                rt.backoff.consecutive_restarts(),
                usage
                    .tasks
                    .map_or_else(|| "-".to_string(), |tasks| tasks.to_string())
            ),
            width
        )
    ));
    out.push_str(&format!(
        "│ {} │\n",
        fit(
            &format!("memory: {}   cpu: {}", memory_text(usage), cpu_text(usage)),
            width
        )
    ));
    out.push_str(&format!(
        "│ {} │\n",
        fit(
            &format!("restart: {}   user: {}", restart_text(rt), user_text(rt)),
            width
        )
    ));
    out.push_str(&format!("╰{}╯\n", "─".repeat(width + 2)));
    out
}

fn unit_type_text(rt: &UnitRuntime) -> String {
    if rt.config.is_target() {
        "target".to_string()
    } else {
        format!("service ({})", service_kind_text(rt.config.service_kind))
    }
}

fn service_kind_text(kind: ServiceKind) -> &'static str {
    match kind {
        ServiceKind::Simple => "simple",
        ServiceKind::Oneshot => "oneshot",
        ServiceKind::Notify => "notify",
        ServiceKind::Forking => "forking",
    }
}

fn state_text(rt: &UnitRuntime) -> String {
    match rt.pid {
        Some(pid) => format!("{} (pid {pid})", rt.state),
        None => rt.state.to_string(),
    }
}

fn restart_text(rt: &UnitRuntime) -> String {
    let policy = match rt.config.restart {
        rinit_core::unit::config::RestartPolicy::No => "no",
        rinit_core::unit::config::RestartPolicy::Always => "always",
        rinit_core::unit::config::RestartPolicy::OnSuccess => "on-success",
        rinit_core::unit::config::RestartPolicy::OnFailure => "on-failure",
        rinit_core::unit::config::RestartPolicy::OnAbnormal => "on-abnormal",
    };
    format!("{policy} ({}s)", rt.config.restart_sec)
}

fn user_text(rt: &UnitRuntime) -> String {
    if rt.config.user.is_empty() {
        "root".to_string()
    } else {
        rt.config.user.clone()
    }
}

fn exec_text(rt: &UnitRuntime) -> String {
    let argv = rt.config.exec_start.argv();
    if argv.is_empty() {
        "-".to_string()
    } else {
        argv.join(" ")
    }
}

fn outcome_text(outcome: Option<ExitOutcome>) -> Option<String> {
    match outcome {
        Some(ExitOutcome::Success) => Some("success".to_string()),
        Some(ExitOutcome::Failure(code)) => Some(format!("exit {code}")),
        Some(ExitOutcome::Signalled(signal)) => Some(format!("signal {signal}")),
        Some(ExitOutcome::TimedOut) => Some("timeout".to_string()),
        None => None,
    }
}

fn memory_text(usage: ResourceUsage) -> String {
    match usage.memory_bytes {
        None => "-".to_string(),
        Some(bytes) if bytes < 1024 => format!("{bytes} B"),
        Some(bytes) if bytes < 1024 * 1024 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        Some(bytes) => format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)),
    }
}

fn cpu_text(usage: ResourceUsage) -> String {
    match usage.cpu_usec {
        None => "-".to_string(),
        Some(usec) if usec < 1_000_000 => format!("{:.1} ms", usec as f64 / 1000.0),
        Some(usec) => format!("{:.2} s", usec as f64 / 1_000_000.0),
    }
}

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < ' ' => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn json_opt(value: &Option<String>) -> String {
    value
        .as_deref()
        .map_or_else(|| "null".to_string(), json_string)
}

fn json_bool(value: Option<bool>) -> String {
    value.map_or_else(|| "null".to_string(), |value| value.to_string())
}

fn fit(value: &str, width: usize) -> String {
    let mut out: String = value.chars().take(width).collect();
    let used = out.chars().count();
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
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
        manager.next_idle_in(),
    ]
    .into_iter()
    .flatten()
    .min();
    // No pending timer: sleep for a long time and let a signal wake us.
    soonest.map_or(std::time::Duration::from_secs(3600), |d| {
        d.max(std::time::Duration::from_millis(1))
    })
}

/// How a boot-time wait for `Type=notify` readiness ended.
enum NotifyWait {
    Ready,
    /// The process exited, or the unit otherwise left `Starting`, without
    /// reporting readiness.
    Failed,
    TimedOut,
}

/// Wait for `unit` to send `READY=1` (plan §1.3(12)).
///
/// Boot cannot use the supervisor's notify arm yet, so it pumps the same hub
/// here. Messages for other units are still delivered to the manager: a notify
/// service that becomes ready while this one is being awaited must not have its
/// readiness dropped.
async fn wait_for_notify(
    manager: &mut UnitManager,
    notify: &mut NotifyHub,
    unit: &UnitName,
    pid: libc::pid_t,
    timeout: Duration,
) -> NotifyWait {
    let deadline = crate::deadline::after(timeout);
    loop {
        match manager.get(unit).map(|rt| rt.state) {
            Some(UnitState::Active | UnitState::ActiveExited) => return NotifyWait::Ready,
            Some(UnitState::Starting) => {}
            _ => return NotifyWait::Failed,
        }

        // A notify service that dies before `READY=1` is a failed start, not a
        // zombie boot is waiting on. Reap it through the manager so its policy
        // and cgroup cleanup still apply.
        match rinit_runtime::syscall::wait::poll_child(pid) {
            Ok(Some(exit)) => {
                if let Some(outcome) = manager.on_child_exit(pid, child_outcome(exit)) {
                    log_outcome(&outcome);
                }
                continue;
            }
            Ok(None) => {}
            Err(e) if e.errno() == libc::EINTR => continue,
            Err(_) => return NotifyWait::Failed,
        }

        if Instant::now() >= deadline {
            let _ = process::kill_process_group(pid, libc::SIGKILL);
            manager.signal_cgroup_members(unit, libc::SIGKILL);
            // Reap the killed process, but leave the state change to the
            // caller's `fail_unit`: going through `on_child_exit` first would
            // let `Restart=` arm a retry that the failed start must not get.
            let _ = readiness::wait_for_exit(pid, Duration::from_secs(5)).await;
            return NotifyWait::TimedOut;
        }

        tokio::select! {
            Some((source, message)) = notify.recv() => {
                if let Some(status) = &message.status {
                    rlog!("{source}: {status}");
                }
                if manager.on_notify(&source, &message) {
                    rlog!("{source} is ready");
                }
            }
            _ = tokio::time::sleep(START_POLL) => {}
        }
    }
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
    activation: &mut Activated,
    started: std::time::Instant,
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

    // Plan §7.9: bind the sockets of every unit that asked to be started by a
    // connection, *before* any level runs - the socket outliving the service is
    // the whole mechanism, and a client that connects to a socket bound a moment
    // later has already been told the service does not exist.
    let activated: Vec<(UnitName, String)> = plan
        .configs
        .iter()
        .filter_map(|config| {
            config
                .socket_activate
                .as_ref()
                .map(|path| (config.name.clone(), path.clone()))
        })
        .collect();
    for (unit, path) in activated {
        match activation.bind(&unit, std::path::Path::new(&path)) {
            Ok(()) => {
                if let Some(fd) = activation.listener_fd(&unit) {
                    units.set_listener(&unit, fd);
                }
            }
            // A socket rinit cannot bind is a failed unit, not a failed boot: the
            // rest of the system has no reason to stop because one service cannot
            // be reached.
            Err(e) => {
                rlog!("error: {unit}: cannot bind {path}: {e}");
                fail_unit(&mut units, &unit);
            }
        }
    }
    // Shutdown walks the startup order backwards (plan §1.3(8)), so record it
    // while the schedule is still at hand.
    units.set_shutdown_order(plan.levels.iter().flatten().cloned().collect());

    let mut started_units = 0usize;
    // Plan §4.3: when the first service was pulled up, which is the number the
    // acceptance list asks for.
    let mut first_service_uptime_ms: Option<u64> = None;
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
            // Plan §7.9: a socket-activated unit is bound, not started. Its
            // service comes up when a connection does.
            if units.listener_fd(name).is_some() {
                rlog!("{name}: waiting for a connection");
                continue;
            }

            // The boot schedule is a set of `Dependency` start requests, so a
            // unit pulled in twice is started once (plan §1.3(11)).
            let effect = units.request_start(name, RequestSource::Dependency);
            let spawned = apply_effect(&mut units, notify, logs, name, effect).await?;
            if spawned {
                started_units += 1;
                if first_service_uptime_ms.is_none() {
                    first_service_uptime_ms = rinit_runtime::time::uptime_ms();
                }
            }
            // A start that already failed has no readiness to wait for, and
            // running `ExecStartPost=` for it would be a command after a start
            // that never happened.
            if units.is_failed(name) {
                continue;
            }

            // `oneshot`/`forking` are ready once the command exits 0; `notify`
            // once it sends `READY=1`; `simple` at exec.
            if readiness::readiness_of(entry.kind) != readiness::Readiness::Immediate
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
            match readiness::readiness_of(*kind) {
                readiness::Readiness::OnReady => {
                    match wait_for_notify(&mut units, notify, name, *pid, timeout).await {
                        NotifyWait::Ready => run_start_post(&units, name).await,
                        NotifyWait::Failed => {
                            rlog!("error: {name} failed before reporting readiness");
                            fail_unit(&mut units, name);
                        }
                        NotifyWait::TimedOut => {
                            rlog!("error: {name} did not report readiness within {timeout_sec}s");
                            units.signal_cgroup_members(name, libc::SIGKILL);
                            fail_unit(&mut units, name);
                        }
                    }
                }
                _ => match readiness::wait_for_exit(*pid, timeout).await {
                    Ok(Some(0)) => match kind {
                        // Plan §7.10: the exit is the middle of a `forking`
                        // unit's start, so the daemon it left behind is what
                        // makes it `Active` - and not being able to find it
                        // fails the start rather than leaving a unit that
                        // claims to be running.
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
                        // And whatever the process group cannot reach: a
                        // `forking` unit that never detached has already started
                        // the daemon it never named, and abandoning the start
                        // must not leave it running (plan §7.10).
                        units.signal_cgroup_members(name, libc::SIGKILL);
                        fail_unit(&mut units, name);
                    }
                },
            }
        }

        // Targets spawn no process (plan §7.2); reaching one means its
        // dependencies are up, which is what releases the next level.
        let targets: Vec<&UnitName> = level
            .iter()
            .filter(|name| manager::config_of(&plan, name).is_some_and(|c| c.is_target()))
            .collect();
        if !targets.is_empty() {
            let mut active = Vec::new();
            let mut failed = Vec::new();
            // Targets spawn nothing, so the level loop has to settle their
            // state explicitly (plan §7.2). `reach_target` refuses to overwrite
            // a Failed that `fail_start` already propagated from a dependency.
            for name in targets {
                match units.reach_target(name) {
                    UnitState::Active => active.push(name.canonical()),
                    UnitState::Failed(_) => failed.push(name.canonical()),
                    _ => {}
                }
            }
            if !active.is_empty() {
                rlog!("level {index} reached: {} active", active.join(", "));
            }
            if !failed.is_empty() {
                rlog!("level {index} failed: {} failed", failed.join(", "));
            }
        }
    }

    rlog!(
        "boot complete: {started_units} unit(s) started from '{}' ({} still running)",
        plan.default_target,
        units.running_count()
    );
    // Plan §4.3's number, and rinit's share of it. `first_service_uptime_ms` is
    // the kernel's clock, so it covers the kernel's own boot too; the elapsed time
    // is the same clock read from a different corner, and the difference is what
    // rinit spent.
    let rinit_ms = started.elapsed().as_millis() as u64;
    match first_service_uptime_ms {
        Some(first) => rlog!(
            "boot timing: first service at {first}ms of kernel uptime (rinit's own boot: {rinit_ms}ms)"
        ),
        None => rlog!("boot timing: rinit's own boot took {rinit_ms}ms (kernel uptime unreadable)"),
    }
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

/// Mount whatever `/etc/fstab` asks for (plan §1.3(9)).
///
/// Three ways for this to do nothing, and all of them are normal: no `--fstab`
/// path (or `none`), no such file, or `nofstab` on the kernel command line. A
/// failure per entry is a warning and the boot carries on - a data partition that
/// will not mount is a degraded system, not a dead one.
async fn report_fstab(cfg: &Config) -> Result<(), RuntimeError> {
    let Some(path) = &cfg.fstab else {
        rlog!("second-wave mounts: disabled");
        return Ok(());
    };
    let cmdline = cmdline();
    if !rinit_runtime::fstab::enabled(&cmdline) {
        rlog!("second-wave mounts: 'nofstab' on the kernel command line");
        return Ok(());
    }

    let path = path.clone();
    let report = tokio::task::spawn_blocking(move || {
        let mut ops = rinit_runtime::RealMounts;
        rinit_runtime::fstab::run_with(&mut ops, &path)
    })
    .await
    .map_err(|e| RuntimeError::Join {
        op: "mount",
        detail: e.to_string(),
    })??;

    for point in &report.mounted {
        rlog!("mounted {point} (fstab)");
    }
    for warning in &report.warnings {
        rlog!("warning: {warning}");
    }
    Ok(())
}

/// The kernel command line, or an empty string when it cannot be read.
///
/// The second wave's veto (`nofstab`) lives there, and a system where
/// `/proc/cmdline` cannot be read is one where the answer is the default rather
/// than a failed boot.
fn cmdline() -> String {
    std::fs::read_to_string("/proc/cmdline").unwrap_or_default()
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

/// Power the machine off after the service tree has stopped.
///
/// Mirrors [`reboot_now`]: unmount what can be unmounted, sync, then
/// `reboot(RB_POWER_OFF)`. The SysRq `o` trigger is the last resort so PID 1
/// never remains alive after a requested poweroff.
pub fn poweroff_now() -> ! {
    rlog!("powering off");
    let report = rinit_runtime::unmount_filesystems();
    for point in &report.unmounted {
        rlog!("unmounted {point}");
    }
    for warning in &report.warnings {
        rlog!("warning: {warning}");
    }
    reboot::sync();
    if let Err(e) = reboot::reboot(reboot::RB_POWER_OFF) {
        rlog!("poweroff failed: {e}; trying sysrq");
        if let Err(e) = reboot::sysrq_poweroff() {
            rlog!("sysrq poweroff failed: {e}; halting");
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

    /// Drive a control request to its reply.
    ///
    /// Production defers a start reply to the supervisor loop; this helper is
    /// the smallest version of that loop, so the tests exercise the same state
    /// machine rather than a second synchronous start path.
    async fn control(
        manager: &mut UnitManager,
        notify: &mut NotifyHub,
        logs: &ServiceLogs,
        request: &Request,
    ) -> (Status, String) {
        let mut activation = Activated::disabled();
        let mut starts = StartCoordinator::default();
        let action = handle_control(
            &test_config(),
            manager,
            notify,
            logs,
            &mut activation,
            &mut starts,
            request,
        )
        .await;
        let mut job = match action {
            ControlAction::Reply(status, text) => return (status, text),
            ControlAction::Start(job) => job,
        };
        let (sender, mut receiver) = oneshot::channel();
        job.reply = Some(sender);
        starts.controls.push(job);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for outcome in starts.reap(manager) {
                log_outcome(&outcome);
            }
            starts.advance(manager, notify, logs);
            match receiver.try_recv() {
                Ok(reply) => return reply,
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    panic!("the start flow dropped its reply")
                }
            }
            assert!(
                Instant::now() < deadline,
                "the start did not finish within 5s"
            );
            tokio::time::sleep(START_POLL).await;
        }
    }

    #[test]
    fn formatted_list_has_a_table_and_machine_readable_shapes() {
        let manager = manager_with("app.service");
        let table = render_list(&manager, ListFormat::Table);
        assert!(table.starts_with("UNIT"), "{table}");
        for header in ["TYPE", "STATE", "PID", "TARGET", "MEM", "CPU"] {
            assert!(table.contains(header), "{table}");
        }
        assert!(table.contains("app.service"), "{table}");

        let plain = render_list(&manager, ListFormat::Plain);
        assert!(plain.contains("app.service: inactive"), "{plain}");

        let json = render_list(&manager, ListFormat::Json);
        assert!(json.starts_with('[') && json.ends_with(']'), "{json}");
        assert!(json.contains("\"unit\":\"app.service\""), "{json}");
        assert!(json.contains("\"memory_bytes\":null"), "{json}");
    }

    #[test]
    fn formatted_status_has_resource_fields_and_json_keys() {
        let manager = manager_with("app.service");
        let app = unit("app.service");

        let systemd =
            describe_unit_formatted(&manager, &app, StatusFormat::Systemd).expect("systemd");
        assert!(systemd.contains("Memory:"), "{systemd}");
        assert!(systemd.contains("CPU:"), "{systemd}");
        assert!(systemd.contains("Restart:"), "{systemd}");

        let plain = describe_unit_formatted(&manager, &app, StatusFormat::Plain).expect("plain");
        assert!(plain.contains("memory:"), "{plain}");
        assert!(plain.contains("cpu:"), "{plain}");
        assert!(plain.contains("target: none"), "{plain}");

        let json = describe_unit_formatted(&manager, &app, StatusFormat::Json).expect("json");
        for key in ["\"memory_bytes\"", "\"cpu_usec\"", "\"tasks\""] {
            assert!(json.contains(key), "{json}");
        }
        assert!(json.starts_with('{') && json.ends_with('}'), "{json}");
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
            let (status, text) = control(&mut manager, &mut notify, &logs, &request).await;
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

        let (status, _) = control(
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

        let (status, text) = control(&mut manager, &mut notify, &logs, &status_request(&app)).await;
        assert_eq!(status, Status::Ok);
        assert!(text.contains("app.service"), "{text}");
        assert!(text.contains("active"), "{text}");
        assert!(text.contains("target: started"), "{text}");

        let (status, text) = control(&mut manager, &mut notify, &logs, &Request::List).await;
        assert_eq!(status, Status::Ok);
        assert!(text.contains("app.service"), "{text}");

        let (status, _) = control(
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

        control(
            &mut manager,
            &mut notify,
            &logs,
            &Request::Start(app.clone()),
        )
        .await;
        let (status, text) = control(
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

    /// `rctl start` must not answer before a `oneshot` has actually finished,
    /// and its `ExecStartPost=` belongs after the command has exited.
    #[tokio::test(flavor = "current_thread")]
    async fn control_start_waits_for_a_oneshot_to_finish() {
        let _guard = crate::test_support::children_lock_async().await;
        let dir =
            std::env::temp_dir().join(format!("rinit-control-oneshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let main_marker = dir.join("main");
        let post_marker = dir.join("post");

        let once = unit("once.service");
        let mut cfg = UnitConfig::new(once.clone());
        cfg.service_kind = ServiceKind::Oneshot;
        cfg.exec_start = rinit_core::ExecCommand::new(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("sleep 0.2; echo main > {}", main_marker.display()),
        ]);
        cfg.exec_start_post = vec![rinit_core::ExecCommand::new(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!(
                "test -f {} && echo post > {}",
                main_marker.display(),
                post_marker.display()
            ),
        ])];
        let mut manager = UnitManager::new();
        manager.add(cfg);
        let mut notify = test_hub("oneshot-start");
        let logs = ServiceLogs::new();

        let (status, text) = control(
            &mut manager,
            &mut notify,
            &logs,
            &Request::Start(once.clone()),
        )
        .await;
        assert_eq!(status, Status::Ok, "{text}");
        assert_eq!(manager.get(&once).unwrap().state, UnitState::ActiveExited);
        assert!(main_marker.exists(), "the command must have run: {text}");
        assert!(
            post_marker.exists(),
            "ExecStartPost must run after the exit, before the reply: {text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A start flow returns to the supervisor while a `NeedsSocket=` path does
    /// not exist yet; the path appearing is what lets the next call spawn.
    #[tokio::test(flavor = "current_thread")]
    async fn a_needs_socket_flow_yields_until_the_path_appears() {
        let _guard = crate::test_support::children_lock_async().await;
        let dir = std::env::temp_dir().join(format!("rinit-flow-socket-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let socket = dir.join("ready.sock");
        let app = unit("app.service");

        let mut cfg = UnitConfig::new(app.clone());
        cfg.exec_start =
            rinit_core::ExecCommand::new(vec!["/bin/sleep".to_string(), "30".to_string()]);
        cfg.restart = RestartPolicy::No;
        cfg.needs_socket = vec![socket.display().to_string()];
        cfg.timeout_start_sec = 5;
        let mut manager = UnitManager::new();
        manager.add(cfg);
        let mut notify = test_hub("flow-socket");
        let logs = ServiceLogs::new();
        let mut aux = BTreeMap::new();

        let mut flow = StartFlow::new(&mut manager, &app, StartCompletion::AtReady).expect("flow");
        assert!(
            flow.advance(&mut manager, &mut notify, &logs, &mut aux)
                .is_none(),
            "the flow must yield to the supervisor while the path is missing"
        );
        assert!(manager.get(&app).unwrap().pid.is_none());

        std::fs::write(&socket, b"").expect("create the socket path");
        assert_eq!(
            flow.advance(&mut manager, &mut notify, &logs, &mut aux),
            Some(Ok(true)),
            "the path appearing lets the start finish"
        );
        let pid = manager.get(&app).unwrap().pid.expect("pid");
        let _ = process::kill_process_group(pid, libc::SIGKILL);
        let _ = rinit_runtime::syscall::wait::reap_children();
        let _ = std::fs::remove_dir_all(&dir);
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

        let (status, text) = control(
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

        let (status, text) = control(
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

        let (status, _) = control(
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
