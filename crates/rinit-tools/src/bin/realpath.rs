//! `realpath` - resolve path names.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use rinit_tools::pathutil;
use rinit_tools::{EXIT_OK, program_name, usage_error};

fn run(args: &[String], program: &str) -> ExitCode {
    let mut allow_missing = false;
    let mut quiet = false;
    let mut zero = false;
    let mut paths: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: realpath [-e|-m] [-q] [-z] PATH...");
                return ExitCode::from(EXIT_OK);
            }
            "-e" | "--canonicalize-existing" => allow_missing = false,
            "-m" | "--canonicalize-missing" => allow_missing = true,
            "-q" | "--quiet" => quiet = true,
            "-z" | "--zero" => zero = true,
            "-L" | "--logical" | "-P" | "--physical" => {}
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
        match pathutil::resolve(Path::new(&path), allow_missing) {
            Ok(resolved) => {
                let text = resolved.display().to_string();
                let result = if zero {
                    stdout
                        .write_all(text.as_bytes())
                        .and_then(|()| stdout.write_all(b"\0"))
                } else {
                    writeln!(stdout, "{text}")
                };
                if let Err(error) = result {
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
    let program = program_name(std::env::args().next().as_deref(), "realpath");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rinit-realpath-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn run_handles_existing_and_missing_paths() {
        let root = temp_root("run");
        let file = root.join("file");
        std::fs::write(&file, b"x").expect("file");
        assert_eq!(
            run(&["-e".to_string(), file.display().to_string()], "realpath"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(
                &["-m".to_string(), root.join("missing").display().to_string()],
                "realpath"
            ),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&[root.join("missing").display().to_string()], "realpath"),
            ExitCode::from(1)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn run_handles_help_and_bad_options() {
        assert_eq!(
            run(&["--help".to_string()], "realpath"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "realpath"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&[], "realpath"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
