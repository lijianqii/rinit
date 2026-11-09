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
mod file_log;
mod parse;

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_core::Status;

const USAGE: &str = "\
Usage: rctl <COMMAND> [ARGS]

Control the rinit PID 1 service manager.

Commands:
  list [--format FMT]     List all known units (default: table)
  status [UNIT] [--tail N] [--format FMT]
                            Show unit status (default: systemd, latest 10 logs)
  start <UNIT>            Start a unit
  stop <UNIT>             Stop a unit
  restart <UNIT>          Restart a unit
  reset-failed <UNIT>     Clear a unit's failed/start-limit state
  log [UNIT] [--tail N] [--file[=PATH]]
                          Show buffered log, or read the persisted file
  reload                  Re-read the unit directory and apply the difference
  reboot                  Stop services and reboot
  poweroff                Stop services and power off

Options:
  -h, --help              Show this help and exit
  -V, --version           Show version and exit
  --format=FMT            list: table|plain|short|json
                          status: systemd|plain|short|json|card
  --json                  alias for --format=json
  --plain                 alias for --format=plain (stable key: value)
  --short                 alias for --format=short
  --card                  status alias for --format=card
  --table                 list alias for --format=table
  --tail N                limit status/log to N recent lines
                          (status defaults to 10; 0 disables status logs)
  --file[=PATH]           log reads the persisted file instead of the ring
                          (default: /var/log/rinit/log)

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
#[cfg(test)]
fn collect_args(args: impl Iterator<Item = OsString>) -> Result<Vec<String>, OsString> {
    args.map(OsString::into_string).collect()
}

/// Keep a non-UTF-8 `--file=PATH` as a path.
///
/// Every other argument rctl takes is a unit name, a number or a keyword, but
/// a log path is arbitrary bytes on Unix. The parser itself remains string
/// based, so the path is replaced by a sentinel and restored after parsing.
const FILE_SENTINEL: &str = "\u{0}rinit-file-override";

fn collect_args_with_file_override(
    args: impl Iterator<Item = OsString>,
) -> Result<(Vec<String>, Option<PathBuf>), OsString> {
    let mut file = None;
    let mut strings = Vec::new();
    for arg in args {
        if arg.to_str().is_none()
            && let Some(path) = arg.as_bytes().strip_prefix(b"--file=")
        {
            file = Some(PathBuf::from(OsStr::from_bytes(path)));
            strings.push(format!("--file={FILE_SENTINEL}"));
            continue;
        }
        strings.push(arg.into_string()?);
    }
    Ok((strings, file))
}

/// Write a reply, treating a closed pipe as success and any other write error
/// as a real failure. Ignoring every error would also hide a full disk.
fn emit(text: &str) -> bool {
    match std::io::stdout().write_all(text.as_bytes()) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => true,
        Err(error) => {
            eprintln!("rctl: cannot write output: {error}");
            false
        }
    }
}

fn main() -> ExitCode {
    let (args, file_override) = match collect_args_with_file_override(std::env::args_os().skip(1)) {
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
            return ExitCode::from(if emit(USAGE) { 0 } else { 1 });
        }
        Ok(parse::Parsed::Version) => {
            let text = format!("rctl {}\n", env!("CARGO_PKG_VERSION"));
            return ExitCode::from(if emit(&text) { 0 } else { 1 });
        }
        Ok(parse::Parsed::Request(request)) => request,
        Ok(parse::Parsed::FileLog(mut log)) => {
            if log.path == Path::new(FILE_SENTINEL)
                && let Some(path) = file_override
            {
                log.path = path;
            }
            return file_log::run(&log);
        }
        Err(message) => {
            eprintln!("rctl: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    match client::send(&request) {
        Ok((Status::Ok, text)) => {
            if !text.is_empty() {
                let output = if text.ends_with('\n') {
                    text.clone()
                } else {
                    format!("{text}\n")
                };
                if !emit(&output) {
                    return ExitCode::from(1);
                }
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
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    #[test]
    fn non_utf8_arguments_are_a_usage_error() {
        let bad = OsString::from_vec(vec![b'x', 0xff]);
        assert_eq!(collect_args(std::iter::once(bad.clone())), Err(bad));
    }

    #[test]
    fn a_non_utf8_file_path_is_preserved() {
        let raw = OsString::from_vec(b"--file=/tmp/log-\xff".to_vec());
        let (args, file) =
            collect_args_with_file_override(std::iter::once(raw)).expect("the path is split out");
        assert_eq!(args, [format!("--file={FILE_SENTINEL}")]);
        assert_eq!(
            file,
            Some(PathBuf::from(OsStr::from_bytes(b"/tmp/log-\xff")))
        );
    }

    #[test]
    fn utf8_arguments_pass_through_unchanged() {
        let args =
            collect_args([OsString::from("status"), OsString::from("a.service")].into_iter())
                .expect("both are UTF-8");
        assert_eq!(args, ["status", "a.service"]);
    }
}
