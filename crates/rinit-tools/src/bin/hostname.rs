//! `hostname` - read or set the kernel hostname.

use std::io;
use std::path::Path;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, EXIT_USAGE, program_name, read_trimmed, usage_error};

const PATH: &str = "/proc/sys/kernel/hostname";

fn read_hostname() -> io::Result<String> {
    read_trimmed(Path::new(PATH))
}

fn write_hostname(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_whitespace())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "hostname must be non-empty and contain no whitespace",
        ));
    }
    std::fs::write(PATH, format!("{name}\n"))
}

fn short_name(name: &str) -> String {
    name.split('.').next().unwrap_or(name).to_string()
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let mut argv = std::env::args_os();
    let argv0 = argv.next().and_then(|arg| arg.into_string().ok());
    let args: Vec<String> = match argv.map(|arg| arg.into_string()).collect::<Result<_, _>>() {
        Ok(args) => args,
        Err(arg) => {
            eprintln!("hostname: argument {arg:?} is not valid UTF-8");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let program = program_name(argv0.as_deref(), "hostname");

    let mut file: Option<String> = None;
    let mut short = false;
    let mut name: Option<String> = None;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Usage: hostname [-s] [-F FILE] [NAME]");
                return ExitCode::from(EXIT_OK);
            }
            "-s" | "--short" => short = true,
            "-F" | "--file" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    return ExitCode::from(usage_error(&program, "-F needs a file"));
                };
                file = Some(path.clone());
            }
            value if value.starts_with('-') => {
                return ExitCode::from(usage_error(&program, &format!("unknown option '{value}'")));
            }
            value => {
                if name.is_some() {
                    return ExitCode::from(usage_error(&program, "too many names"));
                }
                name = Some(value.to_string());
            }
        }
        index += 1;
    }

    if let Some(path) = file {
        if name.is_some() {
            return ExitCode::from(usage_error(&program, "-F and NAME are mutually exclusive"));
        }
        match read_trimmed(Path::new(&path)) {
            Ok(value) => name = Some(value),
            Err(e) => {
                eprintln!("{program}: cannot read {path}: {e}");
                return ExitCode::from(1);
            }
        }
    }

    if let Some(name) = name {
        if let Err(e) = write_hostname(&name) {
            eprintln!("{program}: cannot set hostname: {e}");
            return ExitCode::from(1);
        }
        if short {
            match read_hostname() {
                Ok(value) => println!("{}", short_name(&value)),
                Err(e) => {
                    eprintln!("{program}: cannot read hostname: {e}");
                    return ExitCode::from(1);
                }
            }
        }
        return ExitCode::from(EXIT_OK);
    }

    match read_hostname() {
        Ok(value) => {
            println!("{}", if short { short_name(&value) } else { value });
            ExitCode::from(EXIT_OK)
        }
        Err(e) => {
            eprintln!("{program}: cannot read hostname: {e}");
            ExitCode::from(1)
        }
    }
}
