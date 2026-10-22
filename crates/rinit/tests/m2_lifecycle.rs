//! M2 lifecycle end-to-end on the host (plan §3 M2 acceptance).
//!
//! The qemu system test kills a real console service and checks that rinit
//! restarts it. This is the fast, host-side counterpart for the *breaker* half:
//! a unit that fails immediately must be retried a burst's worth of times and
//! then parked in `Failed(StartLimitHit)` rather than restarting forever.
//!
//! Both tests boot the real binary against a temporary unit directory in
//! `--dev` mode (no mounts, no reboot), so they exercise the same
//! `UnitManager` + reaper path the guest uses.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// A throwaway unit directory that cleans itself up.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("rinit-m2-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp unit dir");
        TempDir(path)
    }

    fn write(&self, name: &str, contents: &str) {
        fs::write(self.0.join(name), contents).expect("write unit file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Poll `condition` until it holds, or fail the test at the deadline.
///
/// A fixed sleep is the flaky half of a timing test: it has to be long enough
/// for the slowest machine and is then paid for on every run, and when it is
/// too short the failure names nothing. Polling keeps the wait proportional to
/// what actually happened and says what never happened when it gives up.
fn wait_for(timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Boot a unit directory in dev mode, wait for the boot to finish, then ask it
/// to shut down.
///
/// Returns everything rinit logged on stdout and stderr.
fn boot_units(units: &TempDir, target: &str) -> String {
    boot_units_with_signal(units, target, libc::SIGTERM)
}

/// As [`boot_units`], but shutting down with `signal`.
fn boot_units_with_signal(units: &TempDir, target: &str, signal: libc::c_int) -> String {
    boot_units_extra(units, target, signal, &[])
}

/// As [`boot_units_with_signal`], with extra command-line arguments.
///
/// The extra arguments exist for `--cgroup-root` (plan §7.3): it is how the
/// host tests point the whole limit path at a directory of ordinary files.
fn boot_units_extra(units: &TempDir, target: &str, signal: libc::c_int, extra: &[&str]) -> String {
    boot_units_inspect(units, target, signal, extra, |_, _| ()).0
}

/// rinit's console, read as it is written.
///
/// The boot is a sequence of events, not a duration: sleeping 1.5 s for it is a
/// guess that is too short on a loaded machine and wasted on an idle one. The
/// reader threads make the sequence observable, so a test can wait for the line
/// it needs instead of for a duration.
struct Console {
    bytes: std::sync::Mutex<Vec<u8>>,
}

impl Console {
    fn new() -> Self {
        Console {
            bytes: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.lock().expect("console lock")).into_owned()
    }

    fn contains(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }

    /// Wait for `needle` to appear, or fail the test with what did appear.
    fn wait_for(&self, needle: &str, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.contains(needle) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out after {timeout:?} waiting for {needle:?} in rinit's console:\n{}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Read one pipe into the shared console until the child closes it.
fn drain_console<R: Read + Send + 'static>(
    mut pipe: R,
    console: std::sync::Arc<Console>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => console
                    .bytes
                    .lock()
                    .expect("console lock")
                    .extend_from_slice(&buf[..n]),
            }
        }
    })
}

/// Boot a unit directory, look at it once the boot is complete, then shut it down.
///
/// `inspect` runs after rinit has printed "boot complete" and before the
/// shutdown signal, so a test can read state that only exists while the boot is
/// running - a service pid that is still alive, or the cgroup files it wrote. It
/// receives the control socket path and the live console, because a condition
/// the test has to *wait* for (a restart being counted, a line about a rate
/// limit) is only observable there; a fixed sleep would be a guess about the
/// machine. It must not panic: panicking there would leave rinit running, so
/// assertions belong after the pair is returned.
fn boot_units_inspect<T>(
    units: &TempDir,
    target: &str,
    signal: libc::c_int,
    extra: &[&str],
    inspect: impl FnOnce(&std::path::Path, &Console) -> T,
) -> (String, T) {
    let bin = env!("CARGO_BIN_EXE_rinit");
    let control_dir = units.0.join("control");
    let control_socket = control_dir.join("control.sock");
    let mut child = Command::new(bin)
        .arg("--dev")
        .arg("--units")
        .arg(&units.0)
        .arg("--default")
        .arg(target)
        .args(extra)
        .env("RINIT_CONTROL_DIR", &control_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rinit");

    let console = std::sync::Arc::new(Console::new());
    let readers = [
        drain_console(
            child.stdout.take().expect("piped stdout"),
            std::sync::Arc::clone(&console),
        ),
        drain_console(
            child.stderr.take().expect("piped stderr"),
            std::sync::Arc::clone(&console),
        ),
    ];

    // rinit prints "boot complete" after the boot levels, whether they all
    // succeeded or one failed; that is the event every caller needs before it
    // looks at the machine.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !console.contains("boot complete:") {
        assert!(
            std::time::Instant::now() < deadline,
            "rinit did not finish booting within 15 s:\n{}",
            console.text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !control_socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "rinit must bind the control socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let inspected = inspect(&control_socket, &console);

    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    let status = child.wait().expect("wait for rinit");
    for reader in readers {
        let _ = reader.join();
    }
    let log = console.text();
    assert!(
        status.success(),
        "rinit should exit cleanly on the shutdown signal, got {status:?}:\n{log}"
    );
    (log, inspected)
}

/// A unit that fails immediately, with `Restart=always`, must be retried and
/// then stopped by the breaker once its burst is exhausted.
#[test]
fn a_crashing_unit_is_restarted_then_the_breaker_trips() {
    let dir = TempDir::new("breaker");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=fault.service\n",
    );
    dir.write(
        "fault.service",
        "[Unit]\nDescription=fails on purpose\n\
         [Service]\nType=simple\nExecStart=/bin/false\n\
         Restart=always\nRestartSec=0\nStartLimitBurst=2\nStartLimitIntervalSec=300\n",
    );

    let (log, ()) = boot_units_inspect(
        &dir,
        "all.target",
        libc::SIGTERM,
        &[],
        |_socket, console| {
            // The breaker trips after the boot levels; the line is the event the
            // assertions below need, so wait for it rather than for a window.
            console.wait_for("fault.service hit its start limit", Duration::from_secs(15));
        },
    );

    assert!(log.contains("boot complete"), "boot must finish:\n{log}");
    assert!(
        log.contains("fault.service will restart in"),
        "a crashed Restart=always unit must be retried:\n{log}"
    );
    assert!(
        log.contains("fault.service hit its start limit (2 failures)"),
        "two failures inside the window must trip the breaker:\n{log}"
    );
}

/// `Restart=no` is the control: the same crash must not schedule a restart.
#[test]
fn a_unit_without_restart_is_left_down() {
    let dir = TempDir::new("no-restart");
    dir.write("all.target", "[Unit]\nType=target\nRequires=once.service\n");
    dir.write(
        "once.service",
        "[Unit]\n[Service]\nType=simple\nExecStart=/bin/false\nRestart=no\n",
    );

    let log = boot_units(&dir, "all.target");

    assert!(log.contains("boot complete"), "boot must finish:\n{log}");
    assert!(
        !log.contains("will restart in"),
        "Restart=no must not schedule a restart:\n{log}"
    );
}

/// Plan §7.2: a target spawns no process, but a completed boot has to leave it
/// `Active` - the boot log already says so, and `rctl` must agree.
#[test]
fn a_target_reaches_active_after_boot() {
    let dir = TempDir::new("target-active");
    dir.write(
        "app.service",
        "[Unit]\n[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=app.service\nAfter=app.service\n",
    );

    let target = rinit_core::UnitName::parse("all.target").expect("target name");
    let (log, status) = boot_units_inspect(&dir, "all.target", libc::SIGTERM, &[], |socket, _| {
        control(
            socket,
            rinit_core::Request::Status {
                unit: Some(target.clone()),
                tail: 0,
            },
        )
        .1
    });

    assert!(
        log.contains("level 1 reached: all.target active"),
        "the boot log must say the target was reached:\n{log}"
    );
    assert!(
        status.contains("state: active"),
        "rctl must report the target active, got:\n{status}"
    );
}

/// Plan §1.3(13) / §7.2: a service that fails after a successful boot must fail
/// the target that hard-requires it, not leave it claiming to be active.
#[test]
fn a_runtime_failure_fails_a_requires_target() {
    let dir = TempDir::new("target-runtime-fail");
    dir.write(
        "app.service",
        "[Unit]\n[Service]\nType=simple\n\
         ExecStart=/bin/sh -c \"sleep 2; exit 1\"\nRestart=no\n",
    );
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=app.service\nAfter=app.service\n",
    );

    let target = rinit_core::UnitName::parse("all.target").expect("target name");
    let (log, status) = boot_units_inspect(&dir, "all.target", libc::SIGTERM, &[], |socket, _| {
        wait_for(Duration::from_secs(10), "all.target to fail", || {
            control(
                socket,
                rinit_core::Request::Status {
                    unit: Some(target.clone()),
                    tail: 0,
                },
            )
            .1
            .contains("state: failed")
        });
        control(
            socket,
            rinit_core::Request::Status {
                unit: Some(target.clone()),
                tail: 0,
            },
        )
        .1
    });

    assert!(
        log.contains("all.target requires app.service, which failed"),
        "the failed dependency must be named:\n{log}"
    );
    assert!(
        status.contains("state: failed"),
        "the target must be failed, got:\n{status}"
    );
}

/// Plan §1.1: `Environment=`, `EnvironmentFile=` and `WorkingDirectory=` must
/// actually reach the child, with `Environment=` beating the file.
#[test]
fn environment_and_working_directory_reach_the_service() {
    let dir = TempDir::new("env");
    dir.write("all.target", "[Unit]\nType=target\nRequires=env.service\n");

    let env_file = dir.0.join("env.conf");
    std::fs::write(&env_file, "ENVFILE=from-file\nOVERRIDE=from-file\n").expect("env file");

    // The service's stdout is captured for `rctl log`, so the probe is written
    // to a file the test reads back rather than grepped out of rinit's log.
    let probe = dir.0.join("probe.txt");
    let unit = format!(
        "[Service]\nType=oneshot\n\
         ExecStart=/bin/sh -c \"echo PROBE MARK=$MARK ENVFILE=$ENVFILE OVERRIDE=$OVERRIDE CWD=$(pwd) >> {}\"\n\
         Environment=MARK=hello OVERRIDE=from-unit\n\
         EnvironmentFile={}\n\
         WorkingDirectory=/tmp\n",
        probe.display(),
        env_file.display()
    );
    dir.write("env.service", &unit);

    let log = boot_units(&dir, "all.target");
    let text = std::fs::read_to_string(&probe).unwrap_or_else(|e| panic!("no probe: {e}\n{log}"));

    assert!(text.contains("PROBE"), "the service must run:\n{text}");
    assert!(
        text.contains("MARK=hello"),
        "Environment= must be exported:\n{text}"
    );
    assert!(
        text.contains("ENVFILE=from-file"),
        "EnvironmentFile= must be read:\n{text}"
    );
    assert!(
        text.contains("OVERRIDE=from-unit"),
        "Environment= must win over the file:\n{text}"
    );
    assert!(
        text.contains("CWD=/tmp"),
        "WorkingDirectory= must be applied:\n{text}"
    );
}

/// Plan §1.1: `ExecStartPre` runs before `ExecStart`, `ExecStartPost` after.
#[test]
fn exec_start_pre_and_post_run_around_the_main_command() {
    let dir = TempDir::new("prepost");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    // Each command appends to one file, so the order is the file's order: the
    // service's own output goes to the log buffer now, not to rinit's console.
    let order = dir.0.join("order.txt");
    let app = format!(
        "[Service]\nType=oneshot\n\
         ExecStartPre=/bin/sh -c \"echo PRE >> {m}\"\n\
         ExecStart=/bin/sh -c \"echo MAIN >> {m}\"\n\
         ExecStartPost=/bin/sh -c \"echo POST >> {m}\"\n",
        m = order.display()
    );
    dir.write("app.service", &app);

    let log = boot_units(&dir, "all.target");
    let text = std::fs::read_to_string(&order).unwrap_or_else(|e| panic!("no marker: {e}\n{log}"));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        ["PRE", "MAIN", "POST"],
        "the commands must run in order"
    );
}

/// Plan §1.1: a failing `ExecStartPre` aborts the start - the main command must
/// not run.
#[test]
fn a_failing_exec_start_pre_prevents_the_start() {
    let dir = TempDir::new("pre-fail");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    let ran = dir.0.join("ran.txt");
    let app = format!(
        "[Service]\nType=oneshot\nExecStartPre=/bin/false\n\
         ExecStart=/bin/sh -c \"echo ran >> {}\"\n",
        ran.display()
    );
    dir.write("app.service", &app);

    let log = boot_units(&dir, "all.target");

    assert!(
        log.contains("ExecStartPre"),
        "the failure must be logged:\n{log}"
    );
    assert!(
        !ran.exists(),
        "ExecStart must not run after a failed ExecStartPre"
    );
}

/// Plan §1.3(13): a unit whose `ExecStart` cannot be started is `Failed`, the
/// boot continues, and everything that `Requires=` it does not start.
#[test]
fn a_unit_that_cannot_start_fails_its_required_dependents() {
    let dir = TempDir::new("requires-fail");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=app.service dependent.service\n",
    );
    dir.write(
        "app.service",
        "[Service]\nType=simple\nExecStart=/nonexistent/rinit-test-binary\n",
    );
    let ran = dir.0.join("dependent.txt");
    dir.write(
        "dependent.service",
        &format!(
            "[Unit]\nRequires=app.service\n\
             [Service]\nType=simple\nExecStart=/bin/sh -c \"echo ran >> {}; sleep 30\"\n",
            ran.display()
        ),
    );

    let log = boot_units(&dir, "all.target");

    assert!(
        log.contains("cannot start"),
        "the broken unit must be reported:\n{log}"
    );
    assert!(!ran.exists(), "a dependent of a failed unit must not start");
}

/// Plan §1.3(8): shutdown stops dependents before their dependencies.
#[test]
fn shutdown_stops_dependents_before_their_dependencies() {
    let dir = TempDir::new("shutdown-order");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=base.service app.service\n",
    );
    dir.write(
        "base.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    dir.write(
        "app.service",
        "[Unit]\nRequires=base.service\n[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );

    let log = boot_units(&dir, "all.target");

    let app = log
        .find("stopping app.service")
        .expect("app must be stopped");
    let base = log
        .find("stopping base.service")
        .expect("base must be stopped");
    assert!(
        app < base,
        "the dependent must stop before its dependency:\n{log}"
    );
}

/// Plan §1.1: starting a unit stops whatever it declares `Conflicts=`, before
/// it starts.
#[test]
fn starting_a_unit_stops_what_it_conflicts_with() {
    let dir = TempDir::new("conflicts");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=alpha.service beta.service\n",
    );
    dir.write(
        "alpha.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    dir.write(
        "beta.service",
        "[Unit]\nConflicts=alpha.service\n[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );

    let log = boot_units(&dir, "all.target");

    assert!(
        log.contains("stopping alpha.service: it conflicts with beta.service"),
        "the conflict must be resolved by stopping alpha:\n{log}"
    );
    assert!(
        log.contains("alpha.service stopped; beta.service may start"),
        "beta must wait for alpha to actually stop:\n{log}"
    );
}

/// Plan §7.1 lists `SIGINT` beside `SIGTERM` as a graceful shutdown, and it was
/// the one of the four signals nothing exercised: the loop handles it on its own
/// arm, so a change that broke only that arm would have shipped green.
#[test]
fn sigint_shuts_down_gracefully_too() {
    let dir = TempDir::new("sigint");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    dir.write(
        "app.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );

    let log = boot_units_with_signal(&dir, "all.target", libc::SIGINT);

    assert!(
        log.contains("received SIGINT"),
        "SIGINT must be handled like SIGTERM:\n{log}"
    );
    assert!(
        log.contains("stopping app.service"),
        "and it must stop the units rather than just exit:\n{log}"
    );
}

/// Plan §7.1: `SIGPWR` is a *fast* shutdown - the per-unit timeout is capped at
/// 2s and the whole shutdown at 5s, so a service with a 30s timeout is still
/// killed promptly.
#[test]
fn sigpwr_shuts_down_within_the_power_budget() {
    let dir = TempDir::new("sigpwr");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    // Ignores SIGTERM and has a long stop timeout; only the power-failure
    // compression gets it killed quickly.
    let app = r#"[Service]
Type=simple
ExecStart=/bin/sh -c "trap '' TERM; while :; do sleep 1; done"
TimeoutStopSec=30
"#;
    dir.write("app.service", app);

    let started = std::time::Instant::now();
    let log = boot_units_with_signal(&dir, "all.target", libc::SIGPWR);
    let elapsed = started.elapsed();

    assert!(
        log.contains("received SIGPWR"),
        "SIGPWR must be handled:\n{log}"
    );
    assert!(
        log.contains("did not stop in time; killed"),
        "the service must still be killed:\n{log}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "power shutdown must beat the 30s timeout, took {elapsed:?}:\n{log}"
    );
}

/// Plan §1.1: `ExecStop=`, when set, is the stop action; the `SIGKILL`
/// escalation still applies if the service does not actually stop.
#[test]
fn exec_stop_runs_as_the_stop_action() {
    let dir = TempDir::new("execstop");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    // The service itself never exits on its own; `ExecStop` just records that
    // it ran, so the timeout has to escalate.
    let app = r#"[Service]
Type=simple
ExecStart=/bin/sh -c "while :; do sleep 1; done"
ExecStop=/bin/sh -c "echo MARK_EXECSTOP"
TimeoutStopSec=1
"#;
    dir.write("app.service", app);

    let log = boot_units(&dir, "all.target");

    assert!(
        log.contains("ExecStop pid"),
        "ExecStop must be the stop action:\n{log}"
    );
    assert!(log.contains("MARK_EXECSTOP"), "ExecStop must run:\n{log}");
    assert!(
        log.contains("did not stop in time; killed"),
        "the timeout must still escalate:\n{log}"
    );
}

/// The control socket's security rules (plan §1.3(10)).
///
/// The socket is `0600` inside a `0700` directory, and the *allowed* path is
/// exercised end to end: in `--dev` the rule is "the user rinit runs as", so an
/// ordinary user can drive a socket under `RINIT_CONTROL_DIR`. That is why M3
/// introduced the variable at all - without it the real dispatch path would
/// only ever be reachable from the guest.
///
/// The stricter production rule (root only, whatever `--dev` says) is asserted
/// as a unit test in `src/control.rs`, where it can be checked for every uid
/// combination rather than for whichever one this process happens to run as.
#[test]
fn the_control_socket_is_owner_only_and_mode_0600() {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new("control");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    dir.write(
        "app.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    let control_dir = dir.0.join("control");

    let bin = env!("CARGO_BIN_EXE_rinit");
    let child = Command::new(bin)
        .arg("--dev")
        .arg("--units")
        .arg(&dir.0)
        .arg("--default")
        .arg("all.target")
        .env("RINIT_CONTROL_DIR", &control_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rinit");

    let socket = control_dir.join("control.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "rinit must bind the control socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The socket is 0600: only the owner may even connect.
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "got {mode:o}");
    let dir_mode = std::fs::metadata(&control_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700, "the control directory must be 0700");

    // Ask it something. `--dev` admits the user rinit runs as, so this client -
    // a separate process, but the same user - must be answered.
    let mut stream = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
    let body = rinit_core::encode_request(&rinit_core::Request::List);
    stream.write_all(&rinit_core::frame(&body)).expect("write");
    let mut prefix = [0u8; rinit_core::LENGTH_PREFIX_BYTES];
    stream.read_exact(&mut prefix).expect("read prefix");
    let len = rinit_core::frame_len(prefix).expect("length");
    let mut reply = vec![0u8; len];
    stream.read_exact(&mut reply).expect("read body");
    let (status, text) = rinit_core::decode_response(&reply).expect("decode");

    assert_eq!(
        status,
        rinit_core::Status::Ok,
        "the invoking user must be admitted in --dev: {text}"
    );
    assert!(
        text.contains("app.service"),
        "the reply must list the running unit, got: {text}"
    );

    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let _ = child.wait_with_output();
}

/// Plan §1.3(12): a `Type=notify` unit is not `Active` until it reports
/// readiness, and the whole path - socket creation, `NOTIFY_SOCKET`, the
/// datagram, the state transition - has to work for a real child.
///
/// The service cannot itself send a datagram from a shell, so the test plays
/// the service's part: it waits for the socket rinit created and sends exactly
/// what `sd_notify` would.
#[test]
fn a_notify_unit_becomes_active_when_it_reports_ready() {
    use std::os::unix::net::UnixDatagram;

    let dir = TempDir::new("notify");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    dir.write(
        "app.service",
        "[Service]\nType=notify\nExecStart=/bin/sh -c \"sleep 30\"\n",
    );
    let notify_dir = dir.0.join("notify");
    let control_dir = dir.0.join("control");
    let control_socket = control_dir.join("control.sock");

    let bin = env!("CARGO_BIN_EXE_rinit");
    let child = Command::new(bin)
        .arg("--dev")
        .arg("--units")
        .arg(&dir.0)
        .arg("--default")
        .arg("all.target")
        .env("RINIT_NOTIFY_DIR", &notify_dir)
        .env("RINIT_CONTROL_DIR", &control_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rinit");

    let socket = notify_dir.join("app.service.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "rinit must create the notify socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The service reports readiness, exactly as `sd_notify(READY=1)` would.
    let client = UnixDatagram::unbound().expect("client socket");
    client.send_to(b"READY=1", &socket).expect("send READY=1");

    // `Active` is the state the READY datagram produces, and the "is ready"
    // line is written in the same pass as the transition - so waiting for the
    // state is waiting for the log line, without guessing a duration.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !control_socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "rinit must bind the control socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_for(
        Duration::from_secs(5),
        "app.service to become active",
        || {
            let (_, text) = control(
                &control_socket,
                rinit_core::Request::Status {
                    unit: Some(rinit_core::UnitName::parse("app.service").expect("name")),
                    tail: 0,
                },
            );
            text.contains("state: active")
        },
    );

    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let out = child.wait_with_output().expect("wait for rinit");
    let mut log = String::from_utf8_lossy(&out.stdout).into_owned();
    log.push_str(&String::from_utf8_lossy(&out.stderr));

    assert!(
        log.contains("app.service is ready"),
        "READY=1 must complete start-up:\n{log}"
    );
}

/// Plan §1.1: a service that ignores `SIGTERM` must still be
/// `SIGKILL`ed once `TimeoutStopSec` expires - otherwise shutdown would leave
/// it running forever.
#[test]
fn a_service_that_ignores_sigterm_is_killed_at_shutdown() {
    let dir = TempDir::new("stubborn");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=stubborn.service\n",
    );
    // The shell ignores SIGTERM and keeps respawning its doomed `sleep`, so the
    // direct child is still alive when the stop timeout expires. rinit's
    // tokenizer only knows `"`, so the shell is handed single quotes to
    // interpret itself (to rinit they are literal characters, not quoting).
    let stubborn = r#"[Service]
Type=simple
ExecStart=/bin/sh -c "trap '' TERM; while :; do sleep 1; done"
Restart=always
TimeoutStopSec=1
"#;
    dir.write("stubborn.service", stubborn);

    let log = boot_units(&dir, "all.target");

    assert!(log.contains("boot complete"), "boot must finish:\n{log}");
    assert!(
        log.contains("stubborn.service did not stop in time; killed"),
        "the stop timeout must escalate to SIGKILL:\n{log}"
    );
    assert!(
        !log.contains("stubborn.service will restart in"),
        "a deliberate stop must not restart:\n{log}"
    );
}

/// Plan §3 M4: a unit file must not be able to abort PID 1.
///
/// Every timeout in a unit file lands in `Instant + Duration`, which *panics*
/// when the sum leaves the representable range, and release builds carry
/// `overflow-checks = true` (deliberately - see the workspace `Cargo.toml`), so
/// a `u64::MAX` timeout used to take init down. The two timeouts below are the
/// ones that are added to an `Instant` on the boot path: the `oneshot`'s
/// `TimeoutStartSec` and the long-running service's `TimeoutStopSec`.
///
/// The assertions are the weak part here - what matters is that the boot
/// survives at all, which it only does if the deadline arithmetic is clamped.
#[test]
fn an_absurd_timeout_does_not_abort_the_boot() {
    let dir = TempDir::new("absurd-timeout");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=timer.service slow.service\n",
    );
    dir.write(
        "timer.service",
        "[Unit]\n\
         [Service]\nType=oneshot\nExecStart=/bin/true\n\
         TimeoutStartSec=18446744073709551615\n",
    );
    dir.write(
        "slow.service",
        "[Unit]\n\
         [Service]\nType=simple\nExecStart=/bin/sleep 300\n\
         TimeoutStopSec=18446744073709551615\nRestart=no\n",
    );

    let log = boot_units(&dir, "all.target");

    assert!(log.contains("boot complete"), "boot must finish:\n{log}");
    assert!(
        log.contains("timer.service completed"),
        "an absurd TimeoutStartSec must not stop the oneshot from running:\n{log}"
    );
    // Shutting down stops `slow.service`, which arms the SIGKILL deadline from
    // `TimeoutStopSec` - the other `Instant` arithmetic site.
    assert!(
        log.contains("stopping slow.service"),
        "the long-running service must be stopped on shutdown:\n{log}"
    );
}

// --- §7.4: the privilege drop, end to end ---------------------------------
//
// These boot the real binary, so they exercise the whole path a unit takes -
// `User=`/`Group=`/`NoNewPrivileges=` resolved, a sandbox built, the child
// started through `fork`+`exec` - rather than the pieces separately. What the
// host *cannot* do is become another user, so "a service really ran as `nobody`"
// is the guest's assertion; what is proved here is that the directives reach the
// child, and that when a drop cannot be carried out the unit fails instead of
// quietly running as root.

/// Whether *this* test process already denies privilege escalation, which some
/// runners do. Used as the baseline for the "no flag asked for" case, so the
/// assertion stays about rinit rather than about the runner.
fn no_new_privs_here() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|status| {
        status
            .lines()
            .any(|l| l.starts_with("NoNewPrivs:") && l.ends_with('1'))
    })
}

/// `NoNewPrivileges=true` must reach the service, and a unit that does not ask
/// for it must be left alone.
#[test]
fn no_new_privileges_reaches_the_service() {
    let dir = TempDir::new("nnp");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=with.service without.service\n",
    );

    let with = dir.0.join("with.txt");
    let without = dir.0.join("without.txt");
    let unit = |flag: &str, probe: &std::path::Path| {
        format!(
            "[Service]\nType=oneshot\nNoNewPrivileges={flag}\n\
             ExecStart=/bin/sh -c \"echo NNP=$(grep ^NoNewPrivs /proc/self/status | cut -f2) > {}\"\n",
            probe.display()
        )
    };
    dir.write("with.service", &unit("true", &with));
    dir.write("without.service", &unit("false", &without));

    let log = boot_units(&dir, "all.target");

    let with_text = std::fs::read_to_string(&with)
        .unwrap_or_else(|e| panic!("the flagged service never wrote its probe: {e}\n{log}"));
    assert!(
        with_text.contains("NNP=1"),
        "NoNewPrivileges=true must reach the child:\n{with_text}"
    );

    let without_text = std::fs::read_to_string(&without)
        .unwrap_or_else(|e| panic!("the unflagged service never wrote its probe: {e}\n{log}"));
    let expected = if no_new_privs_here() { "1" } else { "0" };
    assert!(
        without_text.contains(&format!("NNP={expected}")),
        "a unit that does not ask for the flag must inherit it, not get it:\n{without_text}"
    );
}

/// A `User=` that cannot be resolved must fail the unit, not run it as root.
///
/// The service would write a probe file if it ever ran, so the file's absence is
/// the real assertion: failing closed is only meaningful if the thing that was
/// refused does not happen anyway.
#[test]
fn a_unit_with_an_unknown_user_is_failed_rather_than_run_as_root() {
    let dir = TempDir::new("bad-user");
    let probe = dir.0.join("ran.txt");
    dir.write("all.target", "[Unit]\nType=target\nRequires=bad.service\n");
    dir.write(
        "bad.service",
        &format!(
            "[Service]\nType=oneshot\nUser=rinit-no-such-user-xyz\n\
             ExecStart=/bin/sh -c \"echo RAN > {}\"\n",
            probe.display()
        ),
    );

    let log = boot_units(&dir, "all.target");

    assert!(
        log.contains("User= lookup failed"),
        "the failure must name the lookup that could not be resolved:\n{log}"
    );
    assert!(
        !probe.exists(),
        "a service whose user is unknown must not run (as root or otherwise)"
    );
    assert!(
        log.contains("boot complete"),
        "a unit that cannot start must not abort the rest of the boot:\n{log}"
    );
}

/// When the drop itself cannot be carried out, the unit fails - it does not
/// quietly run as rinit's own user.
///
/// In `--dev` mode rinit is unprivileged, so a numeric `User=65534` cannot be
/// honoured: whichever of `setgid`/`setgroups`/`setuid` the runner's own
/// privileges refuse first fails, and the child stops before `exec`. The guest -
/// where rinit is root - proves the other outcome for the same directives.
#[test]
fn a_drop_that_cannot_be_applied_fails_the_unit_closed() {
    if unsafe { libc::geteuid() } == 0 {
        // As root there is no step to refuse; the guest covers the success path.
        return;
    }
    let dir = TempDir::new("cannot-drop");
    let probe = dir.0.join("ran.txt");
    dir.write("all.target", "[Unit]\nType=target\nRequires=drop.service\n");
    dir.write(
        "drop.service",
        &format!(
            "[Service]\nType=oneshot\nUser=65534\nGroup=65534\n\
             ExecStart=/bin/sh -c \"echo RAN > {}\"\n",
            probe.display()
        ),
    );

    let log = boot_units(&dir, "all.target");

    let named_step = ["setgid failed", "setgroups failed", "setuid failed"]
        .iter()
        .any(|step| log.contains(step));
    assert!(
        named_step,
        "the step that could not be applied must be named:\n{log}"
    );
    assert!(
        !probe.exists(),
        "a service that could not drop privileges must not run at all"
    );
    assert!(
        log.contains("boot complete"),
        "a failed drop must not abort the rest of the boot:\n{log}"
    );
}

/// A capability name this build does not know fails the unit, like an unknown
/// `User=` does: a service that quietly lost the capability its unit file asked
/// it to keep would fail much later, at `bind()` or worse.
#[test]
fn a_unit_with_an_unknown_capability_is_failed_rather_than_started() {
    let dir = TempDir::new("bad-cap");
    let probe = dir.0.join("ran.txt");
    dir.write("all.target", "[Unit]\nType=target\nRequires=bad.service\n");
    dir.write(
        "bad.service",
        &format!(
            "[Service]\nType=oneshot\nCapabilityBoundingSet=RINIT_NO_SUCH_CAPABILITY\n\
             ExecStart=/bin/sh -c \"echo RAN > {}\"\n",
            probe.display()
        ),
    );

    let log = boot_units(&dir, "all.target");

    assert!(
        log.contains("CapabilityBoundingSet= failed"),
        "the failure must name the directive that could not be resolved:\n{log}"
    );
    assert!(
        !probe.exists(),
        "a service with an unresolvable capability must not run"
    );
}

/// The same fail-closed rule for the capability steps themselves. `--dev` runs
/// rinit unprivileged, so dropping the bounding set is refused (`CAP_SETPCAP`);
/// the unit must stop there rather than carry on with the capabilities it was
/// asked to give up. The guest, where rinit is root, proves the other outcome.
#[test]
fn a_capability_drop_that_cannot_be_applied_fails_the_unit_closed() {
    if unsafe { libc::geteuid() } == 0 {
        // As root there is nothing to refuse; the guest covers the success path.
        return;
    }
    let dir = TempDir::new("cannot-drop-caps");
    let probe = dir.0.join("ran.txt");
    dir.write("all.target", "[Unit]\nType=target\nRequires=caps.service\n");
    dir.write(
        "caps.service",
        &format!(
            "[Service]\nType=oneshot\nCapabilityBoundingSet=\n\
             ExecStart=/bin/sh -c \"echo RAN > {}\"\n",
            probe.display()
        ),
    );

    let log = boot_units(&dir, "all.target");

    assert!(
        log.contains("prctl(PR_CAPBSET_DROP) failed"),
        "the failing step must be named:\n{log}"
    );
    assert!(
        !probe.exists(),
        "a service that could not drop its capabilities must not run at all"
    );
    assert!(
        log.contains("boot complete"),
        "a failed capability drop must not abort the rest of the boot:\n{log}"
    );
}

// --- §7.3: cgroup v2 limits, end to end -----------------------------------
//
// cgroupfs provides its own files, so there is nothing to mount and no root
// needed to exercise the limit path: `--cgroup-root` points rinit at a
// directory the test fills in the shape cgroupfs would have. What that stand-in
// cannot do is *remove* a cgroup directory - its attributes are ordinary files,
// so the directory is never empty - which is why the removal side of cleanup is
// asserted in `scripts/system-test.sh`, against a real hierarchy.
//
// Booting the real binary is the point: these prove the directives reach a real
// cgroup's files, that the service's own pid ends up in its `cgroup.procs`, and
// that a limit which cannot be applied fails the unit instead of leaving it
// running unbounded.

/// A throwaway directory standing in for a mounted cgroup v2 hierarchy.
struct FakeCgroup(PathBuf);

impl FakeCgroup {
    /// A directory that [`rinit::cgroup::prepare`]'s probe accepts: the
    /// controllers file is the test for a v2 hierarchy, and `subtree_control`
    /// files exist wherever cgroupfs has them.
    fn new(tag: &str, controllers: &str) -> Self {
        // Deliberately outside the `rinit-m2-*` namespace the unit directories
        // use: a tag that collided with one would delete the unit directory out
        // from under the test.
        let root = std::env::temp_dir().join(format!("rinit-cghier-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("rinit")).expect("create fake hierarchy");
        for dir in [&root, &root.join("rinit")] {
            fs::write(dir.join("cgroup.controllers"), controllers).expect("controllers");
            fs::write(dir.join("cgroup.subtree_control"), "").expect("subtree_control");
        }
        FakeCgroup(root)
    }

    /// The attributes the kernel would provide for a unit's cgroup, created
    /// before rinit runs so its writes find them.
    fn unit(&self, unit: &str, controllers: &str, files: &[&str]) {
        let dir = self.0.join("rinit").join(unit);
        fs::create_dir_all(&dir).expect("create unit cgroup");
        fs::write(dir.join("cgroup.controllers"), controllers).expect("unit controllers");
        for file in files {
            fs::write(dir.join(file), "").expect("attribute");
        }
    }

    fn path(&self) -> &str {
        self.0.to_str().expect("temp dir is UTF-8")
    }
}

impl Drop for FakeCgroup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The whole §7.3 path: the unit's cgroup is created, every directive lands in
/// its own kernel attribute, and the service put *itself* in it before it could
/// fork anything.
#[test]
fn a_unit_with_limits_is_bounded_in_its_own_cgroup() {
    let dir = TempDir::new("units-limits");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=worker.service\n",
    );
    dir.write(
        "worker.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n\
         CgroupCPUMax=50%\nCgroupMemoryMax=32M\nCgroupMemoryHigh=24M\nCgroupTasksMax=64\n",
    );
    let cg = FakeCgroup::new("limits", "cpuset cpu io memory pids\n");
    cg.unit(
        "worker.service",
        "cpu memory pids\n",
        &[
            "cpu.max",
            "memory.max",
            "memory.high",
            "pids.max",
            "cgroup.procs",
            "cgroup.events",
        ],
    );
    fs::write(
        cg.0.join("rinit/worker.service/cgroup.events"),
        "populated 0\n",
    )
    .unwrap();

    // Read while the service is up: that is the only moment the attributes
    // describe a *running* unit, and the only moment its pid can be probed.
    let (log, (cpu, memory_max, memory_high, pids, pid, alive)) = boot_units_inspect(
        &dir,
        "all.target",
        libc::SIGTERM,
        &["--cgroup-root", cg.path()],
        |_socket, _| {
            let read = |rel: &str| fs::read_to_string(cg.0.join(rel)).unwrap_or_default();
            let procs = read("rinit/worker.service/cgroup.procs");
            let pid: libc::pid_t = procs.trim().parse().unwrap_or(-1);
            // SAFETY: signal 0 only probes whether the pid exists.
            let alive = pid > 0 && unsafe { libc::kill(pid, 0) } == 0;
            (
                read("rinit/worker.service/cpu.max"),
                read("rinit/worker.service/memory.max"),
                read("rinit/worker.service/memory.high"),
                read("rinit/worker.service/pids.max"),
                pid,
                alive,
            )
        },
    );

    assert!(
        log.contains("cgroup v2 ready at"),
        "the hierarchy must be recognised:\n{log}"
    );
    assert!(
        log.contains("started worker.service"),
        "the service must start:\n{log}"
    );
    assert_eq!(
        cpu, "50000 100000",
        "50% is a quota against the kernel's own 100000us period"
    );
    assert_eq!(
        memory_max,
        (32u64 * 1024 * 1024).to_string(),
        "CgroupMemoryMax= must be written in bytes"
    );
    assert_eq!(memory_high, (24u64 * 1024 * 1024).to_string());
    assert_eq!(pids, "64");

    // The placement is the child's own write (see the README's deviation note):
    // the pid in the file has to be the service's, and a live one.
    assert!(pid > 0, "the service must have written its real pid: {pid}");
    assert!(
        alive,
        "the placed pid must still be alive while the service runs"
    );
}

/// A hierarchy that is not there is plan §7.3's degrade case: warn, run every
/// unit unlimited, and finish the boot anyway.
#[test]
fn a_missing_hierarchy_warns_and_still_boots() {
    let dir = TempDir::new("units-degrade");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=worker.service\n",
    );
    dir.write(
        "worker.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\nCgroupMemoryMax=32M\n",
    );
    // A directory with no `cgroup.controllers`: not a v2 hierarchy.
    let not_a_hierarchy = TempDir::new("not-a-hierarchy");

    let log = boot_units_extra(
        &dir,
        "all.target",
        libc::SIGTERM,
        &["--cgroup-root", not_a_hierarchy.0.to_str().unwrap()],
    );

    assert!(
        log.contains("no cgroup v2 hierarchy"),
        "the degrade must be reported:\n{log}"
    );
    assert!(
        log.contains("started worker.service"),
        "a unit asking for limits must still start when they cannot be applied:\n{log}"
    );
    assert!(
        log.contains("boot complete"),
        "a missing hierarchy is not a failed boot:\n{log}"
    );
}

/// A limit the kernel *refuses* fails the unit, and the service does not run:
/// starting it anyway would leave it unbounded while its unit file claims
/// otherwise.
#[test]
fn a_limit_the_kernel_refuses_fails_the_start() {
    let dir = TempDir::new("units-fail-closed");
    let probe = dir.0.join("ran.txt");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=worker.service\n",
    );
    dir.write(
        "worker.service",
        &format!(
            "[Service]\nType=oneshot\nCgroupMemoryMax=32M\n\
             ExecStart=/bin/sh -c \"echo RAN > {}\"\n",
            probe.display()
        ),
    );
    // The controller is available and the attribute is there, but it cannot be
    // written: exactly what a refused value looks like from the caller's side
    // (a directory stands in for `EISDIR`).
    let cg = FakeCgroup::new("fail-closed", "cpu memory pids\n");
    cg.unit("worker.service", "cpu memory pids\n", &["cpu.max"]);
    fs::create_dir(cg.0.join("rinit/worker.service/memory.max")).expect("attribute as directory");

    let log = boot_units_extra(
        &dir,
        "all.target",
        libc::SIGTERM,
        &["--cgroup-root", cg.path()],
    );

    assert!(
        log.contains("cannot start"),
        "the failed start must be reported:\n{log}"
    );
    assert!(
        log.contains("memory.max"),
        "the failing attribute must be named:\n{log}"
    );
    assert!(
        !probe.exists(),
        "a service whose limits could not be applied must not run at all"
    );
    assert!(
        log.contains("boot complete"),
        "a failed limit write must not abort the rest of the boot:\n{log}"
    );
}

/// A limit this kernel cannot *express* is not the same thing, and must not stop
/// the unit: `cpu.max` is built only with `CONFIG_CFS_BANDWIDTH`, and a unit
/// written for one kernel has to keep working on another.
#[test]
fn a_limit_this_kernel_cannot_express_does_not_stop_the_unit() {
    let dir = TempDir::new("units-capability-gap");
    let probe = dir.0.join("ran.txt");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=worker.service\n",
    );
    dir.write(
        "worker.service",
        &format!(
            "[Service]\nType=oneshot\nCgroupCPUMax=50%\n\
             ExecStart=/bin/sh -c \"echo RAN > {}\"\n",
            probe.display()
        ),
    );
    // The controller is there, the attribute is not: the kernel does not have
    // CPU bandwidth provisioning.
    let cg = FakeCgroup::new("capability-gap", "cpu memory pids\n");
    cg.unit("worker.service", "cpu memory pids\n", &[]);

    let log = boot_units_extra(
        &dir,
        "all.target",
        libc::SIGTERM,
        &["--cgroup-root", cg.path()],
    );

    assert!(
        log.contains("this kernel does not provide cpu.max"),
        "the skipped limit must be named as a missing attribute:\n{log}"
    );
    assert!(
        !log.contains("cannot start"),
        "a limit the kernel cannot express must not be a failed start:\n{log}"
    );
    assert!(
        probe.exists(),
        "the unit runs, unlimited, with the warning as the record of why"
    );
    assert!(log.contains("boot complete"), "the boot carries on:\n{log}");
}

/// Plan §7.3's OOM link: a service the cgroup's own OOM killer took has to be
/// reported as *that*, and follow the unit's restart policy - a bare `SIGKILL`
/// cannot say whether a timeout, an outside `kill -9` or the memory limit is
/// what happened.
///
/// The two halves are assembled here the way the kernel assembles them in the
/// guest: the service dies of `SIGKILL`, and `memory.events`' `oom_kill` moves.
/// The guest run is what proves the kernel really does both when a service
/// exceeds `memory.max`; this proves rinit reads the pair correctly.
#[test]
fn a_cgroup_oom_kill_is_named_and_the_unit_restarts() {
    let dir = TempDir::new("units-oom");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=worker.service\n",
    );
    dir.write(
        "worker.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\nCgroupMemoryMax=16M\n\
         Restart=on-failure\nRestartSec=0\n",
    );
    let cg = FakeCgroup::new("oom", "cpu memory pids\n");
    cg.unit(
        "worker.service",
        "cpu memory pids\n",
        &[
            "memory.max",
            "cgroup.procs",
            "cgroup.events",
            "memory.events",
        ],
    );
    fs::write(
        cg.0.join("rinit/worker.service/cgroup.events"),
        "populated 0\n",
    )
    .unwrap();
    fs::write(
        cg.0.join("rinit/worker.service/memory.events"),
        "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n",
    )
    .unwrap();

    // The pid is returned rather than asserted: `inspect` must not panic, or
    // rinit is left running.
    let (log, killed) = boot_units_inspect(
        &dir,
        "all.target",
        libc::SIGTERM,
        &["--cgroup-root", cg.path()],
        |socket, _| {
            let procs = fs::read_to_string(cg.0.join("rinit/worker.service/cgroup.procs"))
                .unwrap_or_default();
            let pid: libc::pid_t = procs.trim().parse().unwrap_or(-1);
            // The kernel's record of a kill it performed in this cgroup...
            fs::write(
                cg.0.join("rinit/worker.service/memory.events"),
                "low 0\nhigh 0\nmax 0\noom 1\noom_kill 1\n",
            )
            .unwrap();
            // ...and the death itself. `SIGKILL` is what an OOM kill arrives as,
            // which is exactly why the counter is needed to tell them apart.
            if pid > 0 {
                // SAFETY: signalling the service this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            // The reaper logs the cause and then schedules the restart; the
            // restart counter is the observable result of that same pass, so
            // waiting for it waits for the line the assertion needs.
            wait_for(
                Duration::from_secs(5),
                "the reaper to count the restart",
                || {
                    let (_, text) = control(
                        socket,
                        rinit_core::Request::Status {
                            unit: Some(
                                rinit_core::UnitName::parse("worker.service").expect("name"),
                            ),
                            tail: 0,
                        },
                    );
                    text.contains("restarts: 1")
                },
            );
            pid
        },
    );

    assert!(killed > 0, "the service must have placed its own pid");
    assert!(
        log.contains("killed by the cgroup OOM killer"),
        "the cause of the SIGKILL must be named:\n{log}"
    );
    assert!(
        log.contains("memory.max=16777216"),
        "the memory limit that caused the kill must be named:\n{log}"
    );
    assert!(
        log.contains("worker.service will restart in"),
        "an OOM-killed unit follows its restart policy:\n{log}"
    );
}

/// A `SIGKILL` with the counter unchanged is **not** an OOM kill, and must not
/// be reported as one: `Restart=always` restarts it either way, so the only
/// thing that changes is what the log claims happened.
#[test]
fn a_sigkill_the_cgroup_did_not_make_is_not_reported_as_an_oom_kill() {
    let dir = TempDir::new("units-not-oom");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=worker.service\n",
    );
    dir.write(
        "worker.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\nCgroupMemoryMax=16M\n\
         Restart=always\nRestartSec=0\n",
    );
    let cg = FakeCgroup::new("not-oom", "cpu memory pids\n");
    cg.unit(
        "worker.service",
        "cpu memory pids\n",
        &[
            "memory.max",
            "cgroup.procs",
            "cgroup.events",
            "memory.events",
        ],
    );
    fs::write(
        cg.0.join("rinit/worker.service/memory.events"),
        "oom_kill 0\n",
    )
    .unwrap();

    let (log, killed) = boot_units_inspect(
        &dir,
        "all.target",
        libc::SIGTERM,
        &["--cgroup-root", cg.path()],
        |socket, _| {
            let procs = fs::read_to_string(cg.0.join("rinit/worker.service/cgroup.procs"))
                .unwrap_or_default();
            let pid: libc::pid_t = procs.trim().parse().unwrap_or(-1);
            if pid > 0 {
                // SAFETY: signalling the service this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            wait_for(
                Duration::from_secs(5),
                "the reaper to count the restart",
                || {
                    let (_, text) = control(
                        socket,
                        rinit_core::Request::Status {
                            unit: Some(
                                rinit_core::UnitName::parse("worker.service").expect("name"),
                            ),
                            tail: 0,
                        },
                    );
                    text.contains("restarts: 1")
                },
            );
            pid
        },
    );

    assert!(killed > 0, "the service must have placed its own pid");
    assert!(
        !log.contains("OOM killer"),
        "an unchanged counter must not be reported as an OOM kill:\n{log}"
    );
    assert!(
        log.contains("worker.service will restart in"),
        "the unit still follows its restart policy:\n{log}"
    );
}

// --- §7.5: the hardware watchdog, end to end ------------------

/// A stand-in watchdog device: an ordinary file, never the real one.
///
/// This machine *has* a `/dev/watchdog` (a `misc` device and a `watchdog0`), and
/// opening it would start a hardware timer under whichever developer or CI job
/// ran the test. `--watchdog` exists so the protocol can be pointed somewhere
/// harmless, which is what makes the feed bytes and the magic close observable
/// at all.
struct WatchdogFile(PathBuf);

impl WatchdogFile {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rinit-m2-watchdog-{}-{tag}", std::process::id()));
        fs::write(&path, b"").expect("create the stand-in device");
        WatchdogFile(path)
    }

    fn arg(&self) -> &str {
        self.0.to_str().expect("the temp path is UTF-8")
    }

    fn bytes(&self) -> Vec<u8> {
        fs::read(&self.0).unwrap_or_default()
    }
}

impl Drop for WatchdogFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Plan §7.5's main loop half: the supervisor loop feeds the device as it
/// iterates, and a graceful shutdown writes the magic close.
///
/// The ioctl is refused by a stand-in device (`ENOTTY`), which is also the case
/// the policy layer has to survive: the device stays armed and the feeds use the
/// timeout that was configured, because that is the only number the stand-in
/// will agree to.
#[test]
fn the_supervisor_loop_feeds_the_watchdog_and_shutdown_disarms_it() {
    let dir = TempDir::new("units-watchdog");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=worker.service\n",
    );
    dir.write(
        "worker.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    let device = WatchdogFile::new("fed");

    let log = boot_units_extra(
        &dir,
        "all.target",
        libc::SIGTERM,
        &[
            "--watchdog",
            device.arg(),
            // Half of this is the feed interval: one second, so a 1.5 s boot
            // cannot come out with zero feeds and no flake either.
            "--watchdog-timeout",
            "2",
        ],
    );
    let bytes = device.bytes();

    assert!(
        !log.contains("not touching the watchdog device"),
        "an explicit device is honoured even in --dev:\n{log}"
    );
    assert!(
        log.contains("armed: 2s timeout, feeding every 1s"),
        "the arm line states the cadence the loop will use:\n{log}"
    );
    assert!(
        bytes.len() >= 2,
        "the loop fed the device at least once before shutdown, then disarmed: {bytes:?}"
    );
    assert_eq!(
        bytes.last(),
        Some(&b'V'),
        "the graceful path writes the magic close: {bytes:?}"
    );
    assert_eq!(
        bytes.iter().filter(|b| **b == b'V').count(),
        1,
        "and only one: {bytes:?}"
    );
    assert!(
        bytes[..bytes.len() - 1].iter().all(|b| *b == b'0'),
        "everything before the magic close is a feed: {bytes:?}"
    );
    assert!(log.contains("watchdog disarmed"), "and says so:\n{log}");
}

/// The decision that keeps a developer host off the hardware timer: `--dev`
/// without `--watchdog` must not open the default device at all.
///
/// The assertion is on the decision, not on the file: this machine's
/// `/dev/watchdog` is not writable by the test user, so "nothing was opened"
/// would otherwise be true for the wrong reason.
#[test]
fn dev_mode_does_not_arm_the_default_watchdog_device() {
    let dir = TempDir::new("units-no-watchdog");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=worker.service\n",
    );
    dir.write(
        "worker.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );

    let log = boot_units_extra(&dir, "all.target", libc::SIGTERM, &[]);

    assert!(
        log.contains("dev mode: not touching the watchdog device"),
        "the skip is stated rather than implicit:\n{log}"
    );
    assert!(
        !log.contains("armed:") && !log.contains("watchdog disarmed"),
        "nothing was armed, so nothing is disarmed either:\n{log}"
    );
}

// --- §7.10: Type=forking --------------------------------------

/// Plan §7.10: a `forking` unit's main process exits and the *daemon* it left
/// behind is the unit.
///
/// The daemon here is a `sleep` backgrounded by a shell that then exits, which
/// is the same shape as a double-fork: it is reparented to rinit, it is not the
/// leader of its process group, and nothing but the `PIDFile=` says which process
/// it is. The two assertions that matter are that the boot target was reached
/// only *after* the daemon was located, and that stopping the unit stops the
/// daemon - an untracked daemon would be left running.
#[test]
fn a_forking_unit_is_active_once_its_daemon_is_located() {
    let dir = TempDir::new("forking");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=legacy.service\n",
    );
    let pid_file = std::env::temp_dir().join(format!("rinit-forking-{}.pid", std::process::id()));
    let _ = fs::remove_file(&pid_file);
    dir.write(
        "legacy.service",
        &format!(
            "[Unit]\nDescription=legacy daemon\n[Service]\nType=forking\n\
             PIDFile={}\n\
             ExecStart=/bin/sh -c \"/bin/sleep 30 & echo $! > {}; exit 0\"\n",
            pid_file.display(),
            pid_file.display()
        ),
    );

    let (log, (daemon, alive)) =
        boot_units_inspect(&dir, "all.target", libc::SIGTERM, &[], |_socket, _| {
            let pid = fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
                .unwrap_or(-1);
            // SAFETY: signal 0 only probes whether the pid exists.
            let alive = pid > 0 && unsafe { libc::kill(pid, 0) } == 0;
            (pid, alive)
        });

    assert!(
        alive,
        "the daemon the PIDFile names must be running:\n{log}"
    );
    assert!(
        log.contains("legacy.service: main process exited; daemon is pid"),
        "the unit is active with the *daemon*, not with the process that exited:\n{log}"
    );
    assert!(
        log.contains("reached: all.target active"),
        "the target is only reached once the daemon has been located:\n{log}"
    );

    // The unit was stopped with rinit, and its process is the daemon: a daemon
    // that was only located but not tracked would still be running here.
    wait_for(
        Duration::from_secs(5),
        "the daemon to exit with its unit",
        || {
            // SAFETY: signal 0 only probes whether the pid exists.
            (unsafe { libc::kill(daemon, 0) }) != 0
        },
    );
    // SAFETY: signal 0 only probes whether the pid exists.
    let still_there = unsafe { libc::kill(daemon, 0) } == 0;
    let _ = fs::remove_file(&pid_file);
    assert!(
        !still_there,
        "stopping the unit must stop the daemon it took over (pid {daemon})"
    );
}

/// Plan §7.10's fail-closed rule: a `forking` unit whose daemon cannot be
/// located does not become active, and says why.
///
/// Two ways to fail are covered here - no `PIDFile=` and no cgroup to look in,
/// and a `PIDFile=` naming a pid that is not running - because both would
/// otherwise be a unit that claims to be up with nothing behind it.
#[test]
fn a_forking_unit_whose_daemon_cannot_be_located_fails_its_start() {
    for (tag, unit_body, expected) in [
        (
            "no-pidfile",
            "[Service]\nType=forking\nExecStart=/bin/sh -c \"exit 0\"\n".to_string(),
            "no PIDFile= and no cgroup of its own",
        ),
        (
            "stale-pidfile",
            format!(
                "[Service]\nType=forking\nPIDFile={}\nExecStart=/bin/sh -c \"exit 0\"\n",
                // A pid that cannot be running: the maximum, and rinit itself
                // would have to be it.
                std::env::temp_dir()
                    .join(format!("rinit-forking-stale-{}.pid", std::process::id()))
                    .display()
            ),
            "is not running",
        ),
    ] {
        let dir = TempDir::new(&format!("forking-fail-{tag}"));
        dir.write(
            "all.target",
            "[Unit]\nType=target\nRequires=legacy.service\n",
        );
        dir.write("legacy.service", &unit_body);
        if tag == "stale-pidfile" {
            let path = std::env::temp_dir()
                .join(format!("rinit-forking-stale-{}.pid", std::process::id()));
            fs::write(&path, "2147483647\n").expect("write the stale pid file");
        }

        let log = boot_units(&dir, "all.target");

        assert!(
            log.contains("cannot locate the daemon"),
            "{tag}: the reason must be named:\n{log}"
        );
        assert!(
            log.contains(expected),
            "{tag}: expected a mention of '{expected}':\n{log}"
        );
        assert!(
            log.contains("all.target requires legacy.service, which failed"),
            "{tag}: a unit that never started cannot satisfy its dependents:\n{log}"
        );
        assert!(
            log.contains("boot complete"),
            "{tag}: and none of that may abort the boot:\n{log}"
        );
        if tag == "stale-pidfile" {
            let _ = fs::remove_file(
                std::env::temp_dir()
                    .join(format!("rinit-forking-stale-{}.pid", std::process::id())),
            );
        }
    }
}

// --- §7.9: socket activation ----------------------------------

/// Plan §7.9: rinit binds the socket, and the first connection is what starts the
/// service - which is handed the *listening* socket on fd 3, not an accepted
/// connection.
///
/// The service proves both halves itself: `test -S /proc/self/fd/3` is true only for a
/// descriptor that is a socket, and the probe file is written after it. The
/// connection happens while rinit is up but the unit is still down, which is the
/// state the mechanism exists for.
#[test]
fn a_socket_activated_unit_starts_on_its_first_connection() {
    let dir = TempDir::new("activation");
    let sock = std::env::temp_dir().join(format!("rinit-activation-{}.sock", std::process::id()));
    let probe = std::env::temp_dir().join(format!("rinit-activation-probe-{}", std::process::id()));
    for stale in [&sock, &probe] {
        let _ = fs::remove_file(stale);
    }
    dir.write("all.target", "[Unit]\nType=target\nRequires=api.service\n");
    dir.write(
        "api.service",
        &format!(
            "[Service]\nType=oneshot\nSocketActivate={}\n\
             ExecStart=/bin/sh -c \"test -S /proc/self/fd/3 && echo activated > {} || echo no-fd > {}\"\n",
            sock.display(),
            probe.display(),
            probe.display()
        ),
    );

    let (log, (connected, probed)) =
        boot_units_inspect(&dir, "all.target", libc::SIGTERM, &[], |_socket, _| {
            let connected = std::os::unix::net::UnixStream::connect(&sock).is_ok();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut probed = String::new();
            while std::time::Instant::now() < deadline {
                if let Ok(text) = fs::read_to_string(&probe) {
                    probed = text;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            (connected, probed)
        });
    for stale in [&sock, &probe] {
        let _ = fs::remove_file(stale);
    }

    assert!(connected, "the socket has to be bound by boot time:\n{log}");
    assert!(
        !sock.exists(),
        "and rinit must remove it on shutdown, or a later run cannot bind it again:\n{log}"
    );
    assert!(
        log.contains("api.service: waiting for a connection"),
        "a socket-activated unit is bound, not started:\n{log}"
    );
    assert!(
        log.contains("api.service: started by a connection"),
        "and a connection is what starts it:\n{log}"
    );
    assert_eq!(
        probed.trim(),
        "activated",
        "the service must have the *listening* socket on fd 3, not an accepted one"
    );
}

/// And it is stopped again once nothing has arrived: the socket stays, the memory
/// does not - which is the whole reason to use this instead of a resident
/// service.
#[test]
fn an_idle_socket_activated_unit_is_stopped() {
    let dir = TempDir::new("activation-idle");
    let sock = std::env::temp_dir().join(format!("rinit-idle-{}.sock", std::process::id()));
    let _ = fs::remove_file(&sock);
    dir.write("all.target", "[Unit]\nType=target\nRequires=api.service\n");
    dir.write(
        "api.service",
        &format!(
            "[Service]\nType=simple\nSocketActivate={}\nIdleTimeoutSec=1\n\
             ExecStart=/bin/sleep 30\n",
            sock.display()
        ),
    );

    let log = boot_units_inspect(&dir, "all.target", libc::SIGTERM, &[], |socket, _| {
        // Start it, then leave it alone for longer than its idle timeout. The
        // state names are what prove both halves happened: active means the
        // connection really started it, inactive means the idle rule stopped it.
        let connected = std::os::unix::net::UnixStream::connect(&sock).is_ok();
        let api = || {
            control(
                socket,
                rinit_core::Request::Status {
                    unit: Some(rinit_core::UnitName::parse("api.service").expect("name")),
                    tail: 0,
                },
            )
            .1
        };
        wait_for(
            Duration::from_secs(5),
            "the connection to start api.service",
            || api().contains("state: active"),
        );
        wait_for(
            Duration::from_secs(10),
            "the idle timeout to stop api.service",
            || api().contains("state: inactive"),
        );
        connected
    })
    .0;
    assert!(
        !sock.exists(),
        "an idle socket survives the unit, but not rinit: the file goes on shutdown"
    );
    let _ = fs::remove_file(&sock);

    assert!(
        log.contains("api.service: started by a connection"),
        "the connection starts it:\n{log}"
    );
    assert!(
        log.contains("stopping api.service: idle"),
        "and being idle stops it again:\n{log}"
    );
}

// --- §7.8: sockets as dependencies ----------------------------

/// Plan §7.8: a `NeedsSocket=` unit waits for the *path*, not for the providing
/// unit's state.
///
/// The provider is `Type=simple`, so it is `Active` the moment it is exec'd -
/// long before it creates the socket, which is the window the directive exists to
/// close. The consumer says what it found, so the assertion is about the file the
/// kernel has rather than about intent.
#[test]
fn a_needs_socket_unit_waits_for_the_path() {
    let dir = TempDir::new("socket-dep");
    let path = std::env::temp_dir().join(format!("rinit-socket-dep-{}.sock", std::process::id()));
    // A service's stdout goes to the log ring, not to the console this test
    // reads, so the consumer reports what it found by writing a *file* - the
    // same probe the §7.4 tests use.
    let found = std::env::temp_dir().join(format!("rinit-socket-found-{}", std::process::id()));
    let missing = std::env::temp_dir().join(format!("rinit-socket-missing-{}", std::process::id()));
    for stale in [&path, &found, &missing] {
        let _ = fs::remove_file(stale);
    }
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=sockd.service sockc.service\n",
    );
    dir.write(
        "sockd.service",
        &format!(
            "[Service]\nType=simple\nSockets={}\n\
             ExecStart=/bin/sh -c \"sleep 1; touch {}; sleep 30\"\n",
            path.display(),
            path.display()
        ),
    );
    dir.write(
        "sockc.service",
        &format!(
            "[Service]\nType=oneshot\nNeedsSocket={}\n\
             ExecStart=/bin/sh -c \"test -f {} && touch {} || touch {}\"\n",
            path.display(),
            path.display(),
            found.display(),
            missing.display()
        ),
    );

    let log = boot_units(&dir, "all.target");
    let found_it = found.exists();
    for stale in [&path, &found, &missing] {
        let _ = fs::remove_file(stale);
    }

    assert!(
        found_it && !missing.exists(),
        "the consumer must not start before the socket exists (found: {found_it}):\n{log}"
    );
    // The declaration also puts the consumer in a later *level*, which is what
    // the `Requires=`/`After=` edge the resolution adds is for.
    let provider = log
        .find("plan 0: sockd.service")
        .expect("provider scheduled");
    let consumer = log
        .find("plan 1: sockc.service")
        .expect("consumer scheduled later");
    assert!(
        provider < consumer,
        "the socket declaration has to order the consumer after its provider:\n{log}"
    );
}

/// And one that never appears fails the start, naming the path: a client that
/// came up against a missing socket is the race the directive exists to remove.
#[test]
fn a_needs_socket_that_never_appears_fails_the_start() {
    let dir = TempDir::new("socket-dep-missing");
    let path = std::env::temp_dir().join(format!("rinit-socket-never-{}.sock", std::process::id()));
    let _ = fs::remove_file(&path);
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=sockd.service sockc.service\n",
    );
    dir.write(
        "sockd.service",
        &format!(
            "[Service]\nType=simple\nSockets={}\nExecStart=/bin/sleep 30\n",
            path.display()
        ),
    );
    dir.write(
        "sockc.service",
        &format!(
            "[Service]\nType=oneshot\nNeedsSocket={}\nTimeoutStartSec=1\n\
             ExecStart=/bin/sh -c \"echo should-not-run\"\n",
            path.display()
        ),
    );

    let log = boot_units(&dir, "all.target");

    assert!(
        log.contains(&format!(
            "NeedsSocket={} did not appear in time",
            path.display()
        )),
        "the path has to be named:\n{log}"
    );
    assert!(
        log.contains("all.target requires sockc.service, which failed"),
        "and the unit must not be treated as up:\n{log}"
    );
}

// --- §7.7: template units -------------------------------------

/// Plan §7.7: the specifiers are expanded *before the parser sees the command*,
/// and asking for an instance is what creates it.
///
/// The service echoes what it was handed, so the assertion is about the command
/// line the template produced rather than about what rinit meant to produce -
/// and a second instance shows that instances are units of their own.
#[test]
fn starting_an_instance_expands_its_template() {
    let dir = TempDir::new("template");
    dir.write(
        "all.target",
        "[Unit]
Type=target
Requires=app.service
",
    );
    dir.write(
        "app.service",
        "[Service]
Type=simple
ExecStart=/bin/sleep 30
",
    );
    dir.write(
        "worker@.service",
        "[Service]
Type=oneshot
Environment=INSTANCE=%i
\
         ExecStart=/bin/sh -c \"echo instance=%i name=%n prefix=%p env=$INSTANCE\"\n",
    );
    let (child, socket) = boot_with_control(&dir, "template");
    let unit = |name: &str| rinit_core::UnitName::parse(name).expect("unit name");

    // The template exists, is listed as what it is, and cannot be started.
    let (_, list) = control(&socket, rinit_core::Request::List);
    assert!(list.contains("worker@.service: template"), "got: {list}");
    let (status, text) = control(&socket, rinit_core::Request::Start(unit("worker@.service")));
    assert_eq!(status, rinit_core::Status::Error, "{text}");
    assert!(
        text.contains("is a template; start an instance"),
        "the refusal has to say what to do: {text}"
    );

    // An instance does not exist until someone asks for it.
    let (status, text) = control(
        &socket,
        rinit_core::Request::Status {
            unit: Some(unit("worker@1.service")),
            tail: 0,
        },
    );
    assert_eq!(status, rinit_core::Status::NoSuchUnit, "{text}");

    // Asking for one creates it, expanded.
    let (status, text) = control(
        &socket,
        rinit_core::Request::Start(unit("worker@1.service")),
    );
    assert_eq!(status, rinit_core::Status::Ok, "{text}");
    let expanded = "instance=1 name=worker@1.service prefix=worker env=1";
    wait_for(
        Duration::from_secs(5),
        "worker@1's log to name every specifier",
        || {
            let (_, text) = control(
                &socket,
                rinit_core::Request::Log {
                    unit: Some(unit("worker@1.service")),
                    tail: 0,
                },
            );
            text.contains(expanded)
        },
    );
    let (_, text) = control(
        &socket,
        rinit_core::Request::Log {
            unit: Some(unit("worker@1.service")),
            tail: 0,
        },
    );
    assert!(
        text.contains(expanded),
        "every specifier, expanded before the command ran: {text}"
    );

    // And a second instance is a second unit.
    let (status, _) = control(
        &socket,
        rinit_core::Request::Start(unit("worker@2.service")),
    );
    assert_eq!(status, rinit_core::Status::Ok);
    wait_for(Duration::from_secs(5), "worker@2's log to appear", || {
        let (_, text) = control(
            &socket,
            rinit_core::Request::Log {
                unit: Some(unit("worker@2.service")),
                tail: 0,
            },
        );
        text.contains("instance=2 name=worker@2.service")
    });
    let (_, text) = control(
        &socket,
        rinit_core::Request::Log {
            unit: Some(unit("worker@2.service")),
            tail: 0,
        },
    );
    assert!(
        text.contains("instance=2 name=worker@2.service"),
        "got: {text}"
    );

    stop_rinit(child);
}

// --- §1.3(7): the log file ------------------------------------

/// Plan §1.3(7): the ring can be mirrored to a file, which is what makes a log
/// readable after the process that wrote it is gone.
#[test]
fn the_log_file_mirrors_what_a_service_wrote() {
    let dir = TempDir::new("log-file");
    let path = std::env::temp_dir().join(format!("rinit-m2-log-{}.log", std::process::id()));
    let _ = fs::remove_file(&path);
    dir.write(
        "all.target",
        "[Unit]
Type=target
Requires=app.service
",
    );
    dir.write(
        "app.service",
        "[Service]
Type=oneshot
ExecStart=/bin/echo persisted-marker
",
    );

    let log = boot_units_extra(
        &dir,
        "all.target",
        libc::SIGTERM,
        &["--log-file", path.to_str().expect("UTF-8 path")],
    );
    let written = fs::read_to_string(&path).unwrap_or_default();
    let _ = fs::remove_file(&path);

    assert!(
        log.contains("logging to"),
        "the boot says where it is writing:\n{log}"
    );
    assert!(
        written.contains("app.service: persisted-marker"),
        "the service's own output has to be in the file, got: {written:?}"
    );
}

/// And a path that cannot be opened is a warning rather than a failed boot: the
/// plan calls persistence optional, and an init that refused to start because
/// `/var/log` was read-only would be worse than one with logs only in memory.
#[test]
fn a_log_file_that_cannot_be_opened_is_a_warning_not_a_failure() {
    let dir = TempDir::new("log-file-bad");
    dir.write(
        "all.target",
        "[Unit]
Type=target
Requires=app.service
",
    );
    dir.write(
        "app.service",
        "[Service]
Type=oneshot
ExecStart=/bin/echo still-runs\n",
    );

    // Inside `/proc`, where `create_dir_all` can only fail.
    let log = boot_units_extra(
        &dir,
        "all.target",
        libc::SIGTERM,
        &["--log-file", "/proc/rinit-cannot-create-this/log"],
    );

    assert!(
        log.contains("cannot persist logs"),
        "the degradation has to be named:\n{log}"
    );
    assert!(
        log.contains("boot complete"),
        "and the boot has to carry on:\n{log}"
    );
}

// --- §7.11: a controlling terminal ----------------------------

/// A pty pair: the master end stays with the test, and the slave's path is what
/// a unit's `TTYPath=` names.
///
/// `libc` has no Linux binding for the pty helpers, so this is `/dev/ptmx` plus
/// the two ioctls that stand in for `unlockpt` and `ptsname`, with the values
/// from `asm-generic/ioctls.h`: `TIOCSPTLCK` is `_IOW('T', 0x31, int)` and
/// `TIOCGPTN` is `_IOR('T', 0x30, unsigned int)`.
struct Pty {
    master: libc::c_int,
    slave: String,
}

const TIOCSPTLCK: libc::c_ulong = 0x4004_5431;
const TIOCGPTN: libc::c_ulong = 0x8004_5430;

impl Pty {
    fn open() -> Self {
        // SAFETY: `open` on a character device with scalar flags.
        let master = unsafe { libc::open(c"/dev/ptmx".as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        assert!(master >= 0, "open /dev/ptmx");

        let mut unlock: libc::c_int = 0;
        // SAFETY: `unlock` is an `int` for the duration of the call.
        assert_eq!(
            unsafe { libc::ioctl(master, TIOCSPTLCK, &mut unlock) },
            0,
            "unlock the slave"
        );
        let mut number: libc::c_uint = 0;
        // SAFETY: `number` is an `unsigned int` for the duration of the call.
        assert_eq!(
            unsafe { libc::ioctl(master, TIOCGPTN, &mut number) },
            0,
            "read the slave number"
        );

        // Non-blocking, so `wait_for` polls instead of parking on a read that
        // may never see another byte.
        // SAFETY: scalar `fcntl` calls on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
        unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) };

        Pty {
            master,
            slave: format!("/dev/pts/{number}"),
        }
    }

    fn slave(&self) -> &str {
        &self.slave
    }

    /// Everything the master has to say right now.
    fn drain(&self) -> String {
        let mut buf = [0u8; 1024];
        // SAFETY: a read into a buffer this frame owns, on a non-blocking fd.
        let n = unsafe { libc::read(self.master, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            String::from_utf8_lossy(&buf[..n as usize]).into_owned()
        } else {
            String::new()
        }
    }

    /// Poll the master for `needle` until `timeout`.
    fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut seen = String::new();
        while std::time::Instant::now() < deadline {
            seen.push_str(&self.drain());
            if seen.contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // SAFETY: the descriptor is ours and this is the last use.
        unsafe { libc::close(self.master) };
    }
}

/// Plan §7.11: `TTYPath=`. The service gets that device as its *controlling
/// terminal*, and the assertion is the one thing that cannot be faked: writing
/// to `/dev/tty` only works for a process that has one, so `via-tty` arriving at
/// the master side of the pty means `setsid` + `open` + `TIOCSCTTY` + `dup2` all
/// did what the unit asked for.
#[test]
fn a_unit_with_a_tty_path_gets_it_as_its_controlling_terminal() {
    let dir = TempDir::new("tty");
    let pty = Pty::open();
    dir.write(
        "all.target",
        "[Unit]
Type=target
Requires=tty.service
",
    );
    dir.write(
        "tty.service",
        &format!(
            "[Service]
Type=oneshot
TTYPath={}
             ExecStart=/bin/sh -c \"echo via-tty > /dev/tty; exit 0\"\n",
            pty.slave()
        ),
    );

    let (log, seen) = boot_units_inspect(&dir, "all.target", libc::SIGTERM, &[], |_socket, _| {
        pty.wait_for("via-tty", Duration::from_secs(2))
    });

    assert!(
        seen,
        "the service must have a controlling terminal (read: {:?}):\n{log}",
        pty.drain()
    );
    assert!(
        log.contains("tty.service completed"),
        "and it must have exited 0:\n{log}"
    );
}

/// And a `TTYPath=` that cannot become a controlling terminal fails the start,
/// rather than running a service that believes it owns a terminal.
///
/// `/dev/null` is the case: it opens, and `TIOCSCTTY` answers `ENOTTY` - which is
/// exactly what a mistyped device would do on a real board.
#[test]
fn a_tty_path_that_is_not_a_terminal_fails_the_start() {
    let dir = TempDir::new("tty-bad");
    dir.write(
        "all.target",
        "[Unit]
Type=target
Requires=tty.service
",
    );
    dir.write(
        "tty.service",
        "[Service]
Type=oneshot
TTYPath=/dev/null
         ExecStart=/bin/sh -c \"echo should-not-run > /dev/null\"\n",
    );

    let log = boot_units(&dir, "all.target");

    assert!(
        log.contains("ioctl(TIOCSCTTY) failed"),
        "the failing step has to be named:\n{log}"
    );
    assert!(
        log.contains("all.target requires tty.service, which failed"),
        "and the unit must not be treated as up:\n{log}"
    );
}

// --- §7.6: reloading the unit directory -----------------------

/// Send one request to a `--dev` rinit and read its reply.
///
/// The client half of the protocol is `rctl`'s, but a reload needs several
/// requests in sequence against one running rinit - and a test that shells out to
/// `rctl` for each of them would be testing the CLI, not the reload.
fn control(socket: &std::path::Path, request: rinit_core::Request) -> (rinit_core::Status, String) {
    use std::io::{Read, Write};

    let mut stream = std::os::unix::net::UnixStream::connect(socket).expect("connect to rinit");
    stream
        .write_all(&rinit_core::frame(&rinit_core::encode_request(&request)))
        .expect("write the request");
    let mut prefix = [0u8; rinit_core::LENGTH_PREFIX_BYTES];
    stream
        .read_exact(&mut prefix)
        .expect("read the length prefix");
    let len = rinit_core::frame_len(prefix).expect("frame length");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).expect("read the body");
    rinit_core::decode_response(&body).expect("decode the reply")
}

/// Boot `units` in `--dev` with a control socket, and wait for the socket.
fn boot_with_control(units: &TempDir, tag: &str) -> (std::process::Child, PathBuf) {
    let control_dir = units.0.join(format!("control-{tag}"));
    let child = Command::new(env!("CARGO_BIN_EXE_rinit"))
        .arg("--dev")
        .arg("--units")
        .arg(&units.0)
        .arg("--default")
        .arg("all.target")
        .env("RINIT_CONTROL_DIR", &control_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rinit");

    let socket = control_dir.join("control.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "rinit must bind the control socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (child, socket)
}

/// Ask a `--dev` rinit to stop and wait for it.
fn stop_rinit(child: std::process::Child) {
    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    let _ = child.wait_with_output();
}

/// Plan §7.6: a reload applies the difference between the directory and what is
/// running, while the system keeps running.
///
/// Four cases, and they are the plan's: an added unit the target requires is
/// started, a changed unit that is running keeps its old definition until the
/// next start, a changed unit that is not running takes the new one at once, and
/// a removed unit is stopped and then gone from `rctl`.
#[test]
fn a_reload_applies_the_difference_without_a_restart() {
    let dir = TempDir::new("reload");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    dir.write(
        "app.service",
        "[Unit]\nDescription=first\n[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    dir.write(
        "lonely.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    let (child, socket) = boot_with_control(&dir, "reload");
    let unit = |name: &str| rinit_core::UnitName::parse(name).expect("unit name");

    // A reload that changes nothing still has to say so.
    let (status, text) = control(&socket, rinit_core::Request::Reload);
    assert_eq!(status, rinit_core::Status::Ok, "{text}");
    assert!(text.contains("0 added"), "got: {text}");
    assert!(
        text.contains("3 unchanged"),
        "all three files are unchanged: {text}"
    );

    // Added *and* wanted: the boot target requires it now, so it starts.
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=app.service extra.service\n",
    );
    dir.write(
        "extra.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    let (status, text) = control(&socket, rinit_core::Request::Reload);
    assert_eq!(status, rinit_core::Status::Ok, "{text}");
    assert!(text.contains("registered extra.service"), "got: {text}");
    assert!(
        text.contains("started extra.service"),
        "the boot target requires it: {text}"
    );
    let (status, text) = control(
        &socket,
        rinit_core::Request::Status {
            unit: Some(unit("extra.service")),
            tail: 0,
        },
    );
    assert_eq!(status, rinit_core::Status::Ok, "{text}");
    assert!(text.contains("state: active"), "it must really run: {text}");

    // Changed while running: the old definition stays until the next start.
    dir.write(
        "app.service",
        "[Unit]\nDescription=second\n[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    let (status, text) = control(&socket, rinit_core::Request::Reload);
    assert_eq!(status, rinit_core::Status::Ok, "{text}");
    assert!(
        text.contains("app.service is running; its new definition applies at the next start"),
        "got: {text}"
    );
    let (_, text) = control(
        &socket,
        rinit_core::Request::Status {
            unit: Some(unit("app.service")),
            tail: 0,
        },
    );
    assert!(
        text.contains("description: first"),
        "a running unit keeps the definition it is running: {text}"
    );

    // ...and after a stop and a start, the new one is the one in use.
    let (status, _) = control(&socket, rinit_core::Request::Stop(unit("app.service")));
    assert_eq!(status, rinit_core::Status::Ok);
    wait_for(Duration::from_secs(5), "app.service to stop", || {
        let (_, text) = control(
            &socket,
            rinit_core::Request::Status {
                unit: Some(unit("app.service")),
                tail: 0,
            },
        );
        text.contains("state: inactive")
    });
    let (status, _) = control(&socket, rinit_core::Request::Start(unit("app.service")));
    assert_eq!(status, rinit_core::Status::Ok);
    wait_for(
        Duration::from_secs(5),
        "app.service to start with its new definition",
        || {
            let (_, text) = control(
                &socket,
                rinit_core::Request::Status {
                    unit: Some(unit("app.service")),
                    tail: 0,
                },
            );
            text.contains("description: second")
        },
    );
    let (_, text) = control(
        &socket,
        rinit_core::Request::Status {
            unit: Some(unit("app.service")),
            tail: 0,
        },
    );
    assert!(
        text.contains("description: second"),
        "the deferred definition takes over at the next start: {text}"
    );

    // Removed while running: stopped, then gone from every protocol answer.
    let (status, text) = control(&socket, rinit_core::Request::Start(unit("lonely.service")));
    assert_eq!(status, rinit_core::Status::Ok, "{text}");
    fs::remove_file(dir.0.join("lonely.service")).expect("remove the unit file");
    let (status, text) = control(&socket, rinit_core::Request::Reload);
    assert_eq!(status, rinit_core::Status::Ok, "{text}");
    assert!(
        text.contains("stopped lonely.service: its unit file is gone"),
        "got: {text}"
    );
    assert!(text.contains("removed lonely.service"), "got: {text}");
    let (status, _) = control(&socket, rinit_core::Request::Start(unit("lonely.service")));
    assert_eq!(
        status,
        rinit_core::Status::NoSuchUnit,
        "a removed unit is gone, not merely stopped"
    );
    let (_, text) = control(&socket, rinit_core::Request::List);
    assert!(
        !text.contains("lonely.service"),
        "`list` has to agree with `start`: {text}"
    );

    stop_rinit(child);
}

/// Plan §7.6's boundary: a directory that does not resolve is not applied at all.
///
/// Two ways to break it - a reference to a unit that is gone, and a dependency
/// cycle - and in both cases the reply has to be an error that says the running
/// configuration is untouched, which this checks from the other side by asking
/// what is running afterwards.
#[test]
fn a_reload_the_graph_rejects_changes_nothing() {
    let dir = TempDir::new("reload-rejected");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    dir.write(
        "app.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    let (child, socket) = boot_with_control(&dir, "reload-rejected");
    let unit = |name: &str| rinit_core::UnitName::parse(name).expect("unit name");

    // A `Requires=` with nothing behind it: the reference is dangling.
    fs::remove_file(dir.0.join("app.service")).expect("remove the unit file");
    let (status, text) = control(&socket, rinit_core::Request::Reload);
    assert_eq!(status, rinit_core::Status::Error, "{text}");
    assert!(
        text.contains("the running configuration is unchanged"),
        "got: {text}"
    );
    let (status, text) = control(
        &socket,
        rinit_core::Request::Status {
            unit: Some(unit("app.service")),
            tail: 0,
        },
    );
    assert_eq!(
        status,
        rinit_core::Status::Ok,
        "the old configuration has to survive the rejection: {text}"
    );

    // A cycle: two units that wait for each other.
    dir.write(
        "app.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    dir.write(
        "a.service",
        "[Unit]\nRequires=b.service\n[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    dir.write(
        "b.service",
        "[Unit]\nRequires=a.service\n[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    let (status, text) = control(&socket, rinit_core::Request::Reload);
    assert_eq!(status, rinit_core::Status::Error, "{text}");
    assert!(
        text.contains("the running configuration is unchanged"),
        "got: {text}"
    );
    let (status, _) = control(&socket, rinit_core::Request::Start(unit("a.service")));
    assert_eq!(
        status,
        rinit_core::Status::NoSuchUnit,
        "a rejected reload must not have registered anything either"
    );

    stop_rinit(child);
}

// --- §1.3(9): the second wave ----------------------------------

/// `--dev` means "do not touch mounts", and the second wave is mounts: an
/// `--fstab` naming a real, readable file must still be ignored there.
///
/// The file exists and parses, so this is about the decision rather than about a
/// failed read - which is why the assertion is on the log and not on an error.
#[test]
fn dev_mode_ignores_the_fstab() {
    let dir = TempDir::new("dev-fstab");
    dir.write("all.target", "[Unit]\nType=target\nRequires=app.service\n");
    dir.write(
        "app.service",
        "[Service]\nType=simple\nExecStart=/bin/sleep 30\n",
    );
    let fstab = std::env::temp_dir().join(format!("rinit-dev-fstab-{}", std::process::id()));
    fs::write(&fstab, "tmpfs /mnt/rinit-dev-fstab tmpfs defaults 0 0\n").expect("write fstab");

    let log = boot_units_inspect(
        &dir,
        "all.target",
        libc::SIGTERM,
        &["--fstab", fstab.to_str().expect("utf-8 path")],
        |_socket, _| (),
    )
    .0;
    let _ = fs::remove_file(&fstab);

    assert!(
        !log.contains("fstab"),
        "--dev must not read an fstab at all:\n{log}"
    );
    assert!(
        log.contains("dev mode: skipping first-wave mounts"),
        "and the reason must be in the log, not just the absence of a line:\n{log}"
    );
}

// --- console rate limiting --------------------------------------

/// A service that crash-loops writes as fast as the supervisor can restart it,
/// and rinit's own console is the one log sink with no bound of its own (the
/// ring is per unit and byte-capped, the file mirror truncates). The limiter
/// folds a call site's flood and says so.
///
/// The 72-hour soak is where this came from: the churn unit is deliberately
/// unthrottled, and its console reached 955 MB in five hours, which filled the
/// tmpfs the run was using.
#[test]
fn a_crash_loop_cannot_flood_the_console() {
    let dir = TempDir::new("ratelimit");
    dir.write(
        "all.target",
        "[Unit]\nType=target\nRequires=churn.service\n",
    );
    dir.write(
        "churn.service",
        "[Service]\nType=simple\nExecStart=/bin/true\nRestart=always\nRestartSec=0\n\
         StartLimitBurst=1000000\nStartLimitIntervalSec=0\n",
    );

    let log = boot_units_inspect(
        &dir,
        "all.target",
        libc::SIGTERM,
        &[],
        |_socket, console| {
            // The notice is proof the limiter folded a flood; the old 3 s window
            // was only a guess that one had appeared by then.
            console.wait_for("message(s) suppressed", Duration::from_secs(15));
        },
    )
    .0;

    assert!(
        log.contains("will restart in"),
        "the loop has to have been churning, or this test proves nothing:\n{log}"
    );
    assert!(
        log.contains("message(s) suppressed"),
        "a flood must be folded and reported, not silently dropped:\n{log}"
    );

    // The budget is 40 messages per call site per 5 s, and a restart prints
    // through three call sites - so a 3 s window is tens of lines, plus a notice
    // per window. A limiter that did nothing would put thousands here.
    let lines = log.lines().count();
    assert!(
        lines < 600,
        "a crash loop must not write thousands of lines in 3 s, got {lines}:\n{log}"
    );
}
