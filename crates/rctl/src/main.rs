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

use std::ffi::OsString;
use std::process::ExitCode;

use rinit_core::Status;

const USAGE: &str = "\
Usage: rctl <COMMAND> [ARGS]

Control the rinit PID 1 service manager.

Commands:
  list                    List all known units
  status [UNIT] [--tail N] [--format FMT]
                            Show unit status (default format: systemd)
  start <UNIT>            Start a unit
  stop <UNIT>             Stop a unit
  restart <UNIT>          Restart a unit
  reset-failed <UNIT>     Clear a unit's failed/start-limit state
  log [UNIT] [--tail N]   Show buffered log
  reload                  Re-read the unit directory and apply the difference

Options:
  -h, --help              Show this help and exit
  -V, --version           Show version and exit
  --format=FMT            status format: systemd, plain, short, json, card
  --json                  alias for --format=json
  --plain                 alias for --format=plain (stable key: value)
  --short                 alias for --format=short
  --card                  alias for --format=card
  --tail N                include N recent log lines with status

Exit codes:
  0 success   1 error   2 usage   3 no such unit   4 operation conflict
";

/// The command line as text, or the first argument that is not UTF-8.
///
/// `std::env::args()` panics on a non-UTF-8 argument, and a panic is the wrong
/// answer for a CLI that documents `2` as "bad usage": a script that passes a
/// stray byte should get the same exit code as a typo, not a crash with no
/// message. Collecting `OsString`s and converting here is what keeps that
/// promise; every argument rctl accepts is a unit name, a number or a keyword,
/// so rejecting the odd byte is correct rather than lossy.
fn collect_args(args: impl Iterator<Item = OsString>) -> Result<Vec<String>, OsString> {
    args.map(OsString::into_string).collect()
}

fn main() -> ExitCode {
    let args = match collect_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(argument) => {
            // A command line is user input, so a byte that is not UTF-8 is a
            // usage error like any other - not the panic `std::env::args()`
            // would raise on this machine (exit code 101, no message).
            eprintln!("rctl: argument {argument:?} is not valid UTF-8\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn non_utf8_arguments_are_a_usage_error() {
        let bad = OsString::from_vec(vec![b'x', 0xff]);
        assert_eq!(collect_args(std::iter::once(bad.clone())), Err(bad));
    }

    #[test]
    fn utf8_arguments_pass_through_unchanged() {
        let args =
            collect_args([OsString::from("status"), OsString::from("a.service")].into_iter())
                .expect("both are UTF-8");
        assert_eq!(args, ["status", "a.service"]);
    }
}
