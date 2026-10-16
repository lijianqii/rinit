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
        SETTLE,
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
        SETTLE,
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
        SETTLE,
        libc::SIGTERM,
        &["--cgroup-root", cg.path()],
        || {
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
            // Long enough for the reaper to log the cause and schedule the
            // restart before the harness shuts rinit down.
            std::thread::sleep(Duration::from_millis(400));
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
        SETTLE,
        libc::SIGTERM,
        &["--cgroup-root", cg.path()],
        || {
            let procs = fs::read_to_string(cg.0.join("rinit/worker.service/cgroup.procs"))
                .unwrap_or_default();
            let pid: libc::pid_t = procs.trim().parse().unwrap_or(-1);
            if pid > 0 {
                // SAFETY: signalling the service this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            std::thread::sleep(Duration::from_millis(400));
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
        SETTLE,
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

    let log = boot_units_extra(&dir, "all.target", SETTLE, libc::SIGTERM, &[]);

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
        boot_units_inspect(&dir, "all.target", SETTLE, libc::SIGTERM, &[], || {
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
    std::thread::sleep(Duration::from_millis(300));
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

        let log = boot_units(&dir, "all.target", SETTLE);

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
    std::thread::sleep(Duration::from_millis(400));
    let (status, _) = control(&socket, rinit_core::Request::Start(unit("app.service")));
    assert_eq!(status, rinit_core::Status::Ok);
    std::thread::sleep(Duration::from_millis(400));
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
