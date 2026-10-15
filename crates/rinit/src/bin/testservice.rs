//! `rinit-testservice` - the M0 hardcoded service used to verify PID 1.
//!
//! It prints a start banner and, every second, spawns a short-lived child
//! (`--once`) that it waits for. This exercises `fork`/`exec` and child
//! reaping: rinit's own `SIGCHLD` handler must reap this process when it exits.
//! It exits cleanly on `SIGTERM`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigterm(_sig: libc::c_int) {
    // Async-signal-safe: a single atomic store.
    TERMINATE.store(true, Ordering::SeqCst);
}

fn main() {
    // `--once`: heartbeat child that prints and exits immediately.
    if std::env::args().any(|a| a == "--once") {
        println!("rinit-testservice: heartbeat (pid {})", std::process::id());
        return;
    }

    // `--ticks N`: exit cleanly after N ticks (used for the qemu reap demo).
    let args: Vec<String> = std::env::args().collect();
    let ticks_limit: Option<u64> = args
        .iter()
        .position(|a| a == "--ticks")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());

    // SAFETY: installing a simple SIGTERM handler that stores an atomic flag.
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
    }

    println!(
        "rinit-testservice: main started (pid {})",
        std::process::id()
    );

    let exe = std::env::current_exe().unwrap_or_else(|_| "/sbin/rinit-testservice".into());
    let mut ticks: u64 = 0;
    while !TERMINATE.load(Ordering::SeqCst) {
        match std::process::Command::new(&exe).arg("--once").status() {
            Ok(_) => {}
            Err(e) => eprintln!("rinit-testservice: failed to spawn heartbeat: {e}"),
        }
        ticks += 1;
        if ticks.is_multiple_of(5) {
            println!("rinit-testservice: still alive (tick {ticks})");
        }
        if let Some(limit) = ticks_limit
            && ticks >= limit
        {
            println!("rinit-testservice: exiting after {ticks} ticks");
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("rinit-testservice: SIGTERM received, exiting");
}
