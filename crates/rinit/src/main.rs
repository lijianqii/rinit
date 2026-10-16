//! rinit - PID 1 entry point (plan §3 M0).
//!
//! Uses `fn main()` + `std` (plan §0.2 principle 1): the whole binary starts
//! with a normal `main` and a single-threaded tokio runtime. `#[no_main]` is
//! deliberately deferred until profiling shows `std` is the bottleneck.

mod args;
mod boot;
mod control;
mod log;
mod logs;
mod manager;
mod notify;
mod readiness;
mod service;
#[cfg(test)]
mod test_support;
mod unit_manager;

use std::io::Write;
use std::process::ExitCode;

use log::rlog;
use tokio::runtime::Builder;

fn main() -> ExitCode {
    install_panic_hook();

    let cfg = match args::parse(std::env::args_os().skip(1)) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("rinit: {msg}\n\n{}", args::USAGE);
            return ExitCode::from(2);
        }
    };

    if cfg.show_help {
        print!("{}", args::USAGE);
        return ExitCode::SUCCESS;
    }
    if cfg.show_version {
        println!("rinit {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let runtime = match build_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("rinit: failed to build runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(boot::run(cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if boot::is_pid1() {
                boot::fatal(&e);
            }
            rlog!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Build the single-threaded runtime (plan §2.1①: multi-thread runtime breaks
/// `fork()` safety, so `current_thread` is mandatory).
fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    Builder::new_current_thread().enable_all().build()
}

/// Route panics to a serial-visible sink (plan §2.1⑦).
///
/// With `panic = "abort"` the hook still runs before abort. We try `/dev/kmsg`
/// first (kernel log / serial), then console devices, and finally stderr. This
/// is a best-effort diagnostic path: failures are ignored, never retried.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("\nrinit: PANIC: {info}\n");
        for path in ["/dev/kmsg", "/dev/console", "/dev/ttyS0"] {
            if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(path) {
                let _ = f.write_all(msg.as_bytes());
                let _ = f.flush();
                return;
            }
        }
        let _ = std::io::stderr().write_all(msg.as_bytes());
    }));
}
