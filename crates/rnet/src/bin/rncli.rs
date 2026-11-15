use std::process::ExitCode;

fn main() -> ExitCode {
    rnet::cli::run(std::env::args_os().skip(1))
}
