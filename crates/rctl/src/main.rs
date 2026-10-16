//! rctl - command-line control client for rinit.
//!
//! The client half of the control protocol (plan §3 M3, §1.3(10)). It does no
//! policy: it turns a command line into a [`Request`], sends it to rinit's
//! control socket and prints whatever comes back. The exit code is the
//! server's [`Status`], so `rctl` and rinit cannot disagree about what
//! "unit missing" or "operation conflict" means.
//!
//! It is deliberately synchronous: a CLI waits for one reply and exits, so the
//! client needs no runtime (and rctl pulls in no tokio).

// Production code must not panic (plan §3 M4): a bad command line is a `Result`
// the user sees, not a crash. Test modules are exempt.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]

mod client;
mod parse;

use std::process::ExitCode;

use rinit_core::Status;

const USAGE: &str = "\
Usage: rctl <COMMAND> [ARGS]

Control the rinit PID 1 service manager.

Commands:
  list                    List all known units
  status [UNIT] [--tail N] Show unit status (optionally with recent log)
  start <UNIT>            Start a unit
  stop <UNIT>             Stop a unit
  restart <UNIT>          Restart a unit
  reset-failed <UNIT>     Clear a unit's failed/start-limit state
  log [UNIT] [--tail N]   Show buffered log

Options:
  -h, --help              Show this help and exit
  -V, --version           Show version and exit

Exit codes:
  0 success   1 error   2 usage   3 no such unit   4 operation conflict
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let request = match parse::command_line(&args) {
        Ok(parse::Parsed::Help) => {
            print!("{USAGE}");
            return ExitCode::from(0);
        }
        Ok(parse::Parsed::Version) => {
            println!("rctl {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::from(0);
        }
        Ok(parse::Parsed::Request(request)) => request,
        Err(message) => {
            eprintln!("rctl: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match client::send(&request) {
        Ok((Status::Ok, text)) => {
            if !text.is_empty() {
                println!("{text}");
            }
            ExitCode::from(Status::Ok.exit_code())
        }
        Ok((status, text)) => {
            eprintln!("rctl: {text}");
            ExitCode::from(status.exit_code())
        }
        Err(message) => {
            eprintln!("rctl: {message}");
            ExitCode::from(1)
        }
    }
}
