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
    boot_units_extra(units, target, settle, signal, &[])
}

/// As [`boot_units_with_signal`], with extra command-line arguments.
///
/// The extra arguments exist for `--cgroup-root` (plan §7.3): it is how the
/// host tests point the whole limit path at a directory of ordinary files.
fn boot_units_extra(
    units: &TempDir,
    target: &str,
    settle: Duration,
    signal: libc::c_int,
    extra: &[&str],
) -> String {
    boot_units_inspect(units, target, settle, signal, extra, || ()).0
}

/// Boot a unit directory, look at it while it is up, then shut it down.
///
/// `inspect` runs after the boot has settled and before the shutdown signal, so
/// a test can read state that only exists while the boot is running - a service
/// pid that is still alive, or the cgroup files it wrote. It must not panic:
/// panicking there would leave rinit running, so assertions belong after the
/// pair is returned.
fn boot_units_inspect<T>(
    units: &TempDir,
    target: &str,
    settle: Duration,
    signal: libc::c_int,
    extra: &[&str],
    inspect: impl FnOnce() -> T,
) -> (String, T) {
    let bin = env!("CARGO_BIN_EXE_rinit");
    let child = Command::new(bin)
        .arg("--dev")
        .arg("--units")
        .arg(&units.0)
        .arg("--default")
        .arg(target)
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rinit");

    // Let the unit directory boot and any restart/breaker activity happen.
    std::thread::sleep(settle);
    let inspected = inspect();
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

// --- §7.4: the privilege drop, end to end ---------------------------------
//
// These wait `SETTLE` rather than the tighter windows some tests above use:
// every assertion here is about something the *boot* has to have got to
// (a service started, a spawn refused, the boot finished), and a window that is
// merely usually long enough is how this file grows a flaky test. `SETTLE`
// matches the value the breaker test uses.
//
// These boot the real binary, so they exercise the whole path a unit takes -
// `User=`/`Group=`/`NoNewPrivileges=` resolved, a sandbox built, the child
// started through `fork`+`exec` - rather than the pieces separately. What the
// host *cannot* do is become another user, so "a service really ran as `nobody`"
// is the guest's assertion; what is proved here is that the directives reach the
// child, and that when a drop cannot be carried out the unit fails instead of
// quietly running as root.

/// How long the §7.4 tests below let a boot settle before shutting it down.
const SETTLE: Duration = Duration::from_millis(1500);

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

    let log = boot_units(&dir, "all.target", SETTLE);

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

    let log = boot_units(&dir, "all.target", SETTLE);

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

    let log = boot_units(&dir, "all.target", SETTLE);

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

    let log = boot_units(&dir, "all.target", SETTLE);

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

    let log = boot_units(&dir, "all.target", SETTLE);

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
        SETTLE,
        libc::SIGTERM,
        &["--cgroup-root", cg.path()],
        || {
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
        SETTLE,
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

/// A limit that *cannot* be written fails the unit, and the service does not
/// run: starting it anyway would leave it unbounded while its unit file claims
/// otherwise.
#[test]
fn a_limit_that_cannot_be_written_fails_the_start() {
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
    // The controller is available, but the attribute is not: exactly what a
    // failed write looks like from the caller's side.
    let cg = FakeCgroup::new("fail-closed", "cpu memory pids\n");
    cg.unit("worker.service", "cpu memory pids\n", &[]);

    let log = boot_units_extra(
        &dir,
        "all.target",
        SETTLE,
        libc::SIGTERM,
        &["--cgroup-root", cg.path()],
    );

    assert!(
        log.contains("cannot start"),
        "the failed start must be reported:\n{log}"
    );
    assert!(
        log.contains("cpu.max") || log.contains("memory.max"),
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
