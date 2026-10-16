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

/// Boot a unit directory in dev mode, let it settle, then ask it to shut down.
///
/// Returns everything rinit logged on stdout and stderr.
fn boot_units(units: &TempDir, target: &str, settle: Duration) -> String {
    boot_units_with_signal(units, target, settle, libc::SIGTERM)
}

/// As [`boot_units`], but shutting down with `signal`.
fn boot_units_with_signal(
    units: &TempDir,
    target: &str,
    settle: Duration,
    signal: libc::c_int,
) -> String {
    let bin = env!("CARGO_BIN_EXE_rinit");
    let child = Command::new(bin)
        .arg("--dev")
        .arg("--units")
        .arg(&units.0)
        .arg("--default")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rinit");

    // Let the unit directory boot and any restart/breaker activity happen.
    std::thread::sleep(settle);
    // SAFETY: signalling our own child.
    unsafe { libc::kill(child.id() as libc::pid_t, signal) };

    let out = child.wait_with_output().expect("wait for rinit");
    let mut log = String::from_utf8_lossy(&out.stdout).into_owned();
    log.push_str(&String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "rinit should exit cleanly on the shutdown signal, got {:?}:\n{log}",
        out.status
    );
    log
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

    let log = boot_units(&dir, "all.target", Duration::from_millis(1500));

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

    let log = boot_units(&dir, "all.target", Duration::from_millis(600));

    assert!(log.contains("boot complete"), "boot must finish:\n{log}");
    assert!(
        !log.contains("will restart in"),
        "Restart=no must not schedule a restart:\n{log}"
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

    let log = boot_units(&dir, "all.target", Duration::from_millis(800));
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

    let log = boot_units(&dir, "all.target", Duration::from_millis(900));
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

    let log = boot_units(&dir, "all.target", Duration::from_millis(700));

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

    let log = boot_units(&dir, "all.target", Duration::from_millis(800));

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

    let log = boot_units(&dir, "all.target", Duration::from_millis(700));

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

    let log = boot_units(&dir, "all.target", Duration::from_millis(800));

    assert!(
        log.contains("stopping alpha.service: it conflicts with beta.service"),
        "the conflict must be resolved by stopping alpha:\n{log}"
    );
    assert!(
        log.contains("alpha.service stopped; beta.service may start"),
        "beta must wait for alpha to actually stop:\n{log}"
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
    let log = boot_units_with_signal(&dir, "all.target", Duration::from_millis(700), libc::SIGPWR);
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

    let log = boot_units(&dir, "all.target", Duration::from_millis(700));

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

    let bin = env!("CARGO_BIN_EXE_rinit");
    let child = Command::new(bin)
        .arg("--dev")
        .arg("--units")
        .arg(&dir.0)
        .arg("--default")
        .arg("all.target")
        .env("RINIT_NOTIFY_DIR", &notify_dir)
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
    std::thread::sleep(Duration::from_millis(300));

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

    let log = boot_units(&dir, "all.target", Duration::from_millis(700));

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

    let log = boot_units(&dir, "all.target", Duration::from_millis(700));

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
