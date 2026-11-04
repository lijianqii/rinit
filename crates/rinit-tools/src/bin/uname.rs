//! `uname` - print system information.

use std::ffi::CStr;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Selected {
    sysname: bool,
    nodename: bool,
    release: bool,
    version: bool,
    machine: bool,
    processor: bool,
    hardware: bool,
    os: bool,
}

fn field(value: &[libc::c_char]) -> String {
    // SAFETY: the kernel fills these arrays with NUL-terminated strings.
    unsafe { CStr::from_ptr(value.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn system_name() -> libc::utsname {
    let mut name = unsafe { std::mem::zeroed::<libc::utsname>() };
    // SAFETY: `name` is valid local storage for the call.
    unsafe {
        libc::uname(&mut name);
    }
    name
}

fn fields(selected: Selected, name: &libc::utsname) -> Vec<String> {
    let mut out = Vec::new();
    if selected.sysname {
        out.push(field(&name.sysname));
    }
    if selected.nodename {
        out.push(field(&name.nodename));
    }
    if selected.release {
        out.push(field(&name.release));
    }
    if selected.version {
        out.push(field(&name.version));
    }
    if selected.machine {
        out.push(field(&name.machine));
    }
    if selected.processor {
        out.push("unknown".to_string());
    }
    if selected.hardware {
        out.push("unknown".to_string());
    }
    if selected.os {
        out.push("GNU/Linux".to_string());
    }
    out
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut selected = Selected::default();
    let mut any = false;
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: uname [-asnrvmpoi]");
                return ExitCode::from(EXIT_OK);
            }
            "-a" | "--all" => {
                selected = Selected {
                    sysname: true,
                    nodename: true,
                    release: true,
                    version: true,
                    machine: true,
                    processor: true,
                    hardware: true,
                    os: true,
                };
                any = true;
            }
            "-s" | "--kernel-name" => {
                selected.sysname = true;
                any = true;
            }
            "-n" | "--nodename" => {
                selected.nodename = true;
                any = true;
            }
            "-r" | "--kernel-release" => {
                selected.release = true;
                any = true;
            }
            "-v" | "--kernel-version" => {
                selected.version = true;
                any = true;
            }
            "-m" | "--machine" => {
                selected.machine = true;
                any = true;
            }
            "-p" | "--processor" => {
                selected.processor = true;
                any = true;
            }
            "-i" | "--hardware-platform" => {
                selected.hardware = true;
                any = true;
            }
            "-o" | "--operating-system" => {
                selected.os = true;
                any = true;
            }
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => return ExitCode::from(usage_error(program, "unexpected operand")),
        }
    }
    if !any {
        selected.sysname = true;
    }
    let name = system_name();
    println!("{}", fields(selected, &name).join(" "));
    ExitCode::from(EXIT_OK)
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "uname",
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
    fn default_and_all_fields_are_available() {
        let name = system_name();
        let default = fields(
            Selected {
                sysname: true,
                ..Selected::default()
            },
            &name,
        );
        assert_eq!(default.len(), 1);
        assert!(!default.first().expect("field").is_empty());
        let all = fields(
            Selected {
                sysname: true,
                nodename: true,
                release: true,
                version: true,
                machine: true,
                processor: true,
                hardware: true,
                os: true,
            },
            &name,
        );
        assert_eq!(all.len(), 8);
        assert_eq!(all.get(7).map(String::as_str), Some("GNU/Linux"));
    }

    #[test]
    fn run_handles_help_and_bad_options() {
        assert_eq!(
            run(&["--help".to_string()], "uname"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(run(&["-s".to_string()], "uname"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "uname"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    #[test]
    fn run_renders_every_selector() {
        for flag in [
            "-a", "-s", "-n", "-r", "-v", "-m", "-p", "-i", "-o", "--help",
        ] {
            assert_eq!(
                run(&[flag.to_string()], "uname"),
                ExitCode::from(EXIT_OK),
                "{flag}"
            );
        }
        assert_eq!(
            run(&["--bad".to_string()], "uname"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
