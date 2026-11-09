//! `sysctl` - read and write `/proc/sys` knobs.

use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, read_trimmed, usage_error};

fn key_path(root: &Path, key: &str) -> Result<PathBuf, String> {
    if key.is_empty()
        || key.starts_with('/')
        || key
            .split('.')
            .any(|part| part.is_empty() || part == ".." || part.contains('/'))
    {
        return Err(format!("invalid sysctl key '{key}'"));
    }
    Ok(root.join("proc/sys").join(key.replace('.', "/")))
}

fn read_key(root: &Path, key: &str) -> Result<String, String> {
    let path = key_path(root, key)?;
    read_trimmed(&path).map_err(|e| format!("{key}: {e}"))
}

fn write_key(root: &Path, key: &str, value: &str) -> Result<(), String> {
    let path = key_path(root, key)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{key}: {e}"))?;
    }
    std::fs::write(&path, format!("{value}\n")).map_err(|e| format!("{key}: {e}"))
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>, program: &str) -> io::Result<()> {
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
            if let Err(error) = walk(&path, &key, out, program) {
                eprintln!("{program}: {key}: {error}");
            }
        } else {
            // One unreadable knob must not discard every other value in a
            // `sysctl -a` listing.
            match read_trimmed(&path) {
                Ok(value) => out.push(format!("{key} = {value}")),
                Err(error) => eprintln!("{program}: {key}: {error}"),
            }
        }
    }
    Ok(())
}

fn run(args: &[String], root: &Path, program: &str) -> ExitCode {
    let Some(command) = args.first() else {
        return ExitCode::from(usage_error(program, "a key or -a/-w is required"));
    };
    match command.as_str() {
        "-h" | "--help" => {
            println!("Usage: sysctl [-a] [-w KEY=VALUE] [KEY...]");
            ExitCode::from(EXIT_OK)
        }
        "-a" | "--all" => {
            let mut lines = Vec::new();
            if let Err(e) = walk(&root.join("proc/sys"), "", &mut lines, program) {
                eprintln!("{program}: {e}");
                return ExitCode::from(1);
            }
            for line in lines {
                println!("{line}");
            }
            ExitCode::from(EXIT_OK)
        }
        "-w" | "--write" => {
            let assignments = args.get(1..).unwrap_or_default();
            if assignments.is_empty() {
                return ExitCode::from(usage_error(program, "-w needs KEY=VALUE"));
            }
            let mut failed = false;
            for assignment in assignments {
                let Some((key, value)) = assignment.split_once('=') else {
                    return ExitCode::from(usage_error(program, "-w needs KEY=VALUE"));
                };
                if let Err(e) = write_key(root, key, value) {
                    eprintln!("{program}: {e}");
                    failed = true;
                } else {
                    println!("{key} = {value}");
                }
            }
            ExitCode::from(if failed { 1 } else { EXIT_OK })
        }
        key => {
            let mut fail = false;
            for key in std::iter::once(key).chain(args.iter().skip(1).map(String::as_str)) {
                match read_key(root, key) {
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

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "sysctl",
    );
    let Some(args) = rinit_tools::utf8_args(&program) else {
        return ExitCode::from(rinit_tools::EXIT_USAGE);
    };
    let args: Vec<String> = args.into_iter().skip(1).collect();
    run(&args, Path::new("/"), &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-sysctl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc/sys/kernel")).expect("root");
        root
    }

    #[test]
    fn key_path_rejects_traversal_and_empty_parts() {
        let root = Path::new("/");
        assert!(key_path(root, "kernel.hostname").is_ok());
        for bad in [
            "",
            "..",
            "kernel..hostname",
            "/etc/passwd",
            "kernel/../hostname",
        ] {
            assert!(key_path(root, bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn run_reads_writes_and_lists_a_fake_tree() {
        let root = test_root("rw");
        std::fs::write(root.join("proc/sys/kernel/hostname"), "demo\n").expect("write");
        assert_eq!(
            run(&["kernel.hostname".to_string()], &root, "sysctl"),
            ExitCode::from(0)
        );
        assert_eq!(
            run(
                &["-w".to_string(), "kernel.hostname=new".to_string()],
                &root,
                "sysctl"
            ),
            ExitCode::from(0)
        );
        assert_eq!(read_key(&root, "kernel.hostname").as_deref(), Ok("new"));
        assert_eq!(run(&["-a".to_string()], &root, "sysctl"), ExitCode::from(0));
        assert_eq!(
            run(&["--help".to_string()], &root, "sysctl"),
            ExitCode::from(0)
        );
        assert_eq!(run(&["-w".to_string()], &root, "sysctl"), ExitCode::from(2));
        assert_eq!(
            run(&["-w".to_string(), "bad".to_string()], &root, "sysctl"),
            ExitCode::from(2)
        );
        assert_eq!(run(&[], &root, "sysctl"), ExitCode::from(2));
        assert_eq!(
            run(
                &[
                    "-w".to_string(),
                    "kernel.hostname=first".to_string(),
                    "kernel.hostname=second".to_string(),
                ],
                &root,
                "sysctl"
            ),
            ExitCode::from(0)
        );
        assert_eq!(read_key(&root, "kernel.hostname").as_deref(), Ok("second"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_key_is_a_failure() {
        let root = test_root("missing");
        assert_eq!(
            run(&["kernel.nope".to_string()], &root, "sysctl"),
            ExitCode::from(1)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
