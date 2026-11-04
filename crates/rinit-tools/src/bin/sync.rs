//! `sync` - flush filesystem caches.

use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

fn run(args: &[String], program: &str) -> ExitCode {
    if let Some(arg) = args.first() {
        if arg == "--help" {
            println!("Usage: sync");
            return ExitCode::from(EXIT_OK);
        }
        return ExitCode::from(usage_error(program, &format!("unexpected operand '{arg}'")));
    }
    // SAFETY: `sync` has no arguments and no failure mode exposed to userspace.
    unsafe {
        libc::sync();
    }
    ExitCode::from(EXIT_OK)
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "sync",
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
    fn run_accepts_no_arguments_and_help() {
        assert_eq!(run(&[], "sync"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--help".to_string()], "sync"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["extra".to_string()], "sync"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
