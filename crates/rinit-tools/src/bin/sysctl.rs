//! `sysctl` - read and write `/proc/sys` knobs.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, read_trimmed, usage_error};

const ROOT: &str = "/proc/sys";

fn key_path(key: &str) -> Result<PathBuf, String> {
    if key.is_empty()
        || key.starts_with('/')
        || key
            .split('.')
            .any(|part| part.is_empty() || part == ".." || part.contains('/'))
    {
        return Err(format!("invalid sysctl key '{key}'"));
    }
    Ok(Path::new(ROOT).join(key.replace('.', "/")))
}

fn read_key(key: &str) -> Result<String, String> {
    let path = key_path(key)?;
    read_trimmed(&path).map_err(|e| format!("{key}: {e}"))
}

fn write_key(key: &str, value: &str) -> Result<(), String> {
    let path = key_path(key)?;
    std::fs::write(&path, format!("{value}\n")).map_err(|e| format!("{key}: {e}"))
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) -> io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let key = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}.{name}")
        };
        if path.is_dir() {
            walk(&path, &key, out)?;
        } else {
            out.push(format!("{key} = {}", read_trimmed(&path)?));
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "sysctl");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        return ExitCode::from(usage_error(&program, "a key or -a/-w is required"));
    };
    match command.as_str() {
        "-h" | "--help" => {
            println!("Usage: sysctl [-a] [-w KEY=VALUE] [KEY...]");
            ExitCode::from(EXIT_OK)
        }
        "-a" | "--all" => {
            let mut lines = Vec::new();
            if let Err(e) = walk(Path::new(ROOT), "", &mut lines) {
                eprintln!("{program}: {e}");
                return ExitCode::from(1);
            }
            for line in lines {
                println!("{line}");
            }
            ExitCode::from(EXIT_OK)
        }
        "-w" | "--write" => {
            let Some(assignment) = args.get(1) else {
                return ExitCode::from(usage_error(&program, "-w needs KEY=VALUE"));
            };
            let Some((key, value)) = assignment.split_once('=') else {
                return ExitCode::from(usage_error(&program, "-w needs KEY=VALUE"));
            };
            if let Err(e) = write_key(key, value) {
                eprintln!("{program}: {e}");
                return ExitCode::from(1);
            }
            println!("{key} = {value}");
            ExitCode::from(EXIT_OK)
        }
        key => {
            let mut fail = false;
            for key in std::iter::once(key).chain(args.iter().skip(1).map(String::as_str)) {
                match read_key(key) {
                    Ok(value) => println!("{key} = {value}"),
                    Err(e) => {
                        eprintln!("{program}: {e}");
                        fail = true;
                    }
                }
            }
            ExitCode::from(if fail { 1 } else { EXIT_OK })
        }
    }
}
