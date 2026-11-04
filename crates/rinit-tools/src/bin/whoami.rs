//! `whoami` - print the effective user name.

use std::process::ExitCode;

use rinit_tools::users;
use rinit_tools::{EXIT_OK, program_name, usage_error};

fn run(args: &[String], program: &str) -> ExitCode {
    if let Some(arg) = args.first() {
        if arg == "--help" {
            println!("Usage: whoami");
            return ExitCode::from(EXIT_OK);
        }
        return ExitCode::from(usage_error(program, &format!("unexpected operand '{arg}'")));
    }
    // SAFETY: `geteuid` has no preconditions.
    let uid = unsafe { libc::geteuid() };
    match users::user_name(uid) {
        Some(name) => {
            println!("{name}");
            ExitCode::from(EXIT_OK)
        }
        None => {
            eprintln!("{program}: cannot find name for user ID {uid}");
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "whoami",
    );
    let Some(args) = rinit_tools::utf8_args(&program) else {
        return ExitCode::from(rinit_tools::EXIT_USAGE);
    };
    let args: Vec<String> = args.into_iter().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_handles_help_and_operands() {
        assert_eq!(
            run(&["--help".to_string()], "whoami"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["extra".to_string()], "whoami"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(run(&[], "whoami"), ExitCode::from(EXIT_OK));
    }
}
