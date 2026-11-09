//! `dmesg` - dump the kernel ring buffer.

use std::io::Write;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

const BUFFER_BYTES: usize = 1024 * 1024;

fn read_kernel_log() -> std::io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; BUFFER_BYTES];
    // SYSLOG_ACTION_READ_ALL = 3; the kernel returns the number of bytes copied.
    // SAFETY: the buffer is valid writable storage for its full length.
    let count = unsafe {
        libc::klogctl(
            3,
            buffer.as_mut_ptr().cast::<libc::c_char>(),
            buffer.len() as libc::c_int,
        )
    };
    if count < 0 {
        return Err(std::io::Error::last_os_error());
    }
    buffer.truncate(count as usize);
    Ok(buffer)
}

fn run(args: &[String], program: &str, read: impl Fn() -> std::io::Result<Vec<u8>>) -> ExitCode {
    if let Some(arg) = args.first() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Usage: dmesg");
                return ExitCode::from(EXIT_OK);
            }
            other => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{other}'")));
            }
        }
    }
    match read() {
        Ok(bytes) => {
            let mut stdout = std::io::stdout().lock();
            match stdout.write_all(&bytes) {
                Ok(()) => ExitCode::from(EXIT_OK),
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                    ExitCode::from(EXIT_OK)
                }
                Err(error) => {
                    eprintln!("{program}: cannot write output: {error}");
                    ExitCode::from(1)
                }
            }
        }
        Err(e) => {
            eprintln!("{program}: cannot read kernel log: {e}");
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
        "dmesg",
    );
    let Some(args) = rinit_tools::utf8_args(&program) else {
        return ExitCode::from(rinit_tools::EXIT_USAGE);
    };
    let args: Vec<String> = args.into_iter().skip(1).collect();
    run(&args, &program, read_kernel_log)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_handles_options_and_read_errors() {
        assert_eq!(
            run(&["--help".to_string()], "dmesg", || Ok(Vec::new())),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "dmesg", || Ok(Vec::new())),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&[], "dmesg", || Ok(b"hello\n".to_vec())),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&[], "dmesg", || Err(std::io::Error::other("nope"))),
            ExitCode::from(1)
        );
    }
}
