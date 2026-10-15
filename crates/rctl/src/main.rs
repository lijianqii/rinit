//! rctl - command-line control client for rinit.
//!
//! M0 is a stub: it exposes `--help`/`--version` and the subcommand surface so
//! the packaging and exit-code contract are in place. The Unix-socket control
//! protocol and real commands land in M3 (plan §3 M3, §1.3(10)).

use std::process::ExitCode;

/// Exit codes (plan §3 M3).
#[allow(dead_code)] // documented contract; wired up in M3
mod exit {
    pub const OK: u8 = 0;
    pub const ERROR: u8 = 1;
    pub const USAGE: u8 = 2;
    pub const NO_UNIT: u8 = 3;
    pub const CONFLICT: u8 = 4;
}

const USAGE: &str = "\
Usage: rctl <COMMAND> [ARGS]

Control the rinit PID 1 service manager.

Commands (implemented from M3):
  list                    List all known units
  status [UNIT]           Show unit status (optionally with --tail)
  start <UNIT>            Start a unit
  stop <UNIT>             Stop a unit
  restart <UNIT>          Restart a unit
  reset-failed <UNIT>     Clear a unit's failed/start-limit state
  log [UNIT]              Show buffered logs

Options:
  -h, --help              Show this help and exit
  -V, --version           Show version and exit
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            print!("{USAGE}");
            ExitCode::from(exit::OK)
        }
        Some("-V") | Some("--version") => {
            println!("rctl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::from(exit::OK)
        }
        Some(cmd) => {
            eprintln!("rctl: '{cmd}' is not implemented yet (lands in M3)");
            ExitCode::from(exit::ERROR)
        }
    }
}
