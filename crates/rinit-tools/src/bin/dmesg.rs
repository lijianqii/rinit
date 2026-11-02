//! `dmesg` - dump the kernel ring buffer.

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

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "dmesg");
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Usage: dmesg");
                return ExitCode::from(EXIT_OK);
            }
            other => {
                return ExitCode::from(usage_error(&program, &format!("unknown option '{other}'")));
            }
        }
    }
    match read_kernel_log() {
        Ok(bytes) => {
            print!("{}", String::from_utf8_lossy(&bytes));
            ExitCode::from(EXIT_OK)
        }
        Err(e) => {
            eprintln!("{program}: cannot read kernel log: {e}");
            ExitCode::from(1)
        }
    }
}
