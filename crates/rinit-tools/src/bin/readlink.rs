//! `readlink` - print symbolic link targets or resolved paths.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use rinit_tools::pathutil;
use rinit_tools::{EXIT_OK, program_name, usage_error};

enum Mode {
    Target,
    Resolve(bool),
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut mode = Mode::Target;
    let mut newline = true;
    let mut quiet = false;
    let mut paths: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: readlink [-fenmq] PATH...");
                return ExitCode::from(EXIT_OK);
            }
            "-f" | "--canonicalize" => mode = Mode::Resolve(true),
            "-e" | "--canonicalize-existing" => mode = Mode::Resolve(false),
            "-m" | "--canonicalize-missing" => mode = Mode::Resolve(true),
            "-n" | "--no-newline" => newline = false,
            "-q" | "--quiet" => quiet = true,
            "-s" | "--silent" => quiet = true,
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => paths.push(value.to_string()),
        }
    }
    if paths.is_empty() {
        return ExitCode::from(usage_error(program, "a path operand is required"));
    }
    let mut failed = false;
    let mut stdout = std::io::stdout().lock();
    for path in paths {
        let result = match mode {
            Mode::Target => fs::read_link(Path::new(&path)),
            Mode::Resolve(allow_missing) => pathutil::resolve(Path::new(&path), allow_missing),
        };
        match result {
            Ok(target) => {
                let text = target.display().to_string();
                let write = if newline {
                    writeln!(stdout, "{text}")
                } else {
                    stdout.write_all(text.as_bytes())
                };
                if let Err(error) = write {
                    if !quiet {
                        eprintln!("{program}: {error}");
                    }
                    failed = true;
                }
            }
            Err(error) => {
                if !quiet {
                    eprintln!("{program}: {path}: {error}");
                }
                failed = true;
            }
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "readlink");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rinit-readlink-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn run_reads_targets_and_resolves_paths() {
        let root = temp_root("run");
        let target = root.join("target");
        let link = root.join("link");
        fs::write(&target, b"x").expect("target");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        assert_eq!(
            run(&[link.display().to_string()], "readlink"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["-f".to_string(), link.display().to_string()], "readlink"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&[root.join("missing").display().to_string()], "readlink"),
            ExitCode::from(1)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn run_handles_help_and_bad_options() {
        assert_eq!(
            run(&["--help".to_string()], "readlink"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "readlink"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&[], "readlink"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
