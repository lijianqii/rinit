//! `poweroff` - ask PID 1 to stop services and power off.

use std::process::ExitCode;

use rinit_core::{Request, Status};
use rinit_tools::{EXIT_OK, program_name, usage_error};

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "poweroff",
    );
    let Some(args) = rinit_tools::utf8_args(&program) else {
        return ExitCode::from(rinit_tools::EXIT_USAGE);
    };
    let args: Vec<String> = args.into_iter().skip(1).collect();
    run(&args, &program)
}

fn run(args: &[String], program: &str) -> ExitCode {
    rinit_tools::restore_sigpipe();
    if let Some(arg) = args.first() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Usage: poweroff [-f]");
                return ExitCode::from(EXIT_OK);
            }
            // `-f` is accepted for compatibility; this implementation
            // always goes through PID 1 and never bypasses the service tree.
            "-f" | "--force" => {}
            other => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{other}'")));
            }
        }
    }
    match rinit_tools::control::send(&Request::Poweroff) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rinit_core::{
        ENV_CONTROL_DIR, LENGTH_PREFIX_BYTES, decode_request, encode_response, frame, frame_len,
    };
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    fn fake_server(status: Status, reply: &'static str) -> (PathBuf, std::thread::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!(
            "rinit-poweroff-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let listener = UnixListener::bind(dir.join("control.sock")).expect("listener");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
            stream.read_exact(&mut prefix).expect("prefix");
            let len = frame_len(prefix).expect("length");
            let mut body = vec![0u8; len];
            stream.read_exact(&mut body).expect("body");
            assert!(matches!(decode_request(&body), Ok(Request::Poweroff)));
            let response = encode_response(status, reply);
            stream.write_all(&frame(&response)).expect("reply");
        });
        (dir, handle)
    }

    #[test]
    fn run_handles_help_and_bad_options() {
        assert_eq!(
            run(&["--help".to_string()], "poweroff"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "poweroff"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }

    #[test]
    fn run_sends_the_request_and_reports_the_reply() {
        let (dir, handle) = fake_server(Status::Ok, "requested");
        // SAFETY: this test binary has no other control-directory reader.
        unsafe { std::env::set_var(ENV_CONTROL_DIR, &dir) };
        let code = run(&[], "poweroff");
        // SAFETY: as above.
        unsafe { std::env::remove_var(ENV_CONTROL_DIR) };
        handle.join().expect("server");
        assert_eq!(code, ExitCode::from(0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
