//! End-to-end smoke test for the `rinit` binary in dev mode (plan §3 M0).
//!
//! Runs the real binary with `--dev` (no mounts, no reboot), lets it launch a
//! hardcoded service, and verifies it starts, reaps, and shuts down cleanly on
//! SIGTERM. This is a host/x86 substitute for the aarch64 qemu run.

use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn dev_mode_starts_service_and_shuts_down_on_sigterm() {
    let bin = env!("CARGO_BIN_EXE_rinit");

    let child = Command::new(bin)
        .arg("--dev")
        // A service that stays alive so we can exercise SIGTERM → reap.
        .arg("--test-service")
        .arg("/bin/sleep 5")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rinit");

    // Let it boot, launch the service, and exercise a reap cycle.
    std::thread::sleep(Duration::from_millis(600));

    // Ask it to shut down (SIGTERM is handled, not fatal, in dev mode).
    let pid = child.id() as libc::pid_t;
    // SAFETY: sending SIGTERM to our own child.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    let out = child.wait_with_output().expect("wait rinit");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("starting"),
        "expected boot banner, got: {stdout}"
    );
    assert!(
        stdout.contains("launched test service"),
        "expected service launch, got: {stdout}"
    );
    assert!(
        stdout.contains("received SIGTERM"),
        "expected clean shutdown, got: {stdout}"
    );
    assert!(out.status.success(), "rinit exited with {:?}", out.status);
}

#[test]
fn version_flag_prints_version() {
    let bin = env!("CARGO_BIN_EXE_rinit");
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .expect("run rinit --version");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("rinit"), "got: {stdout}");
}
