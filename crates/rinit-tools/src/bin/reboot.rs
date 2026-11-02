//! `reboot` - ask PID 1 to stop services and reboot.

use std::process::ExitCode;

use rinit_core::{Request, Status};
use rinit_tools::{EXIT_OK, program_name, usage_error};

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "reboot");
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Usage: reboot");
                return ExitCode::from(EXIT_OK);
            }
            other => {
                return ExitCode::from(usage_error(&program, &format!("unknown option '{other}'")));
            }
        }
    }
    match rinit_tools::control::send(&Request::Reboot) {
        Ok((Status::Ok, text)) => {
            if !text.is_empty() {
                println!("{text}");
            }
            ExitCode::from(0)
        }
        Ok((status, text)) => {
            eprintln!("{program}: {text}");
            ExitCode::from(status.exit_code())
        }
        Err(e) => {
            eprintln!("{program}: {e}");
            ExitCode::from(1)
        }
    }
}
