//! `realpath` - resolve path names.

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

use rinit_tools::pathutil;
use rinit_tools::{EXIT_OK, program_name, usage_error};

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut allow_missing = false;
    let mut quiet = false;
    let mut zero = false;
    let mut paths: Vec<PathBuf> = Vec::new();
    let marker = args.iter().position(|arg| arg.as_ref() == OsStr::new("--"));
    let (options_args, trailing): (&[A], &[A]) = match marker {
        Some(index) => (
            args.get(..index).unwrap_or(&[]),
            args.get(index + 1..).unwrap_or(&[]),
        ),
        None => (args, &[] as &[A]),
    };
    for arg in options_args {
        match arg.as_ref().to_str() {
            Some("--help") => {
                println!("Usage: realpath [-e|-m|-P] [-q] [-z] PATH...");
                return ExitCode::from(EXIT_OK);
            }
            Some("-e" | "--canonicalize-existing") => allow_missing = false,
            Some("-m" | "--canonicalize-missing") => allow_missing = true,
            Some("-q" | "--quiet") => quiet = true,
            Some("-z" | "--zero") => zero = true,
            // Physical resolution is the only mode implemented; `-P` is the
            // default and is accepted for compatibility. `-L` would silently
            // mean something else, and the tool contract says an unsupported
            // option must fail rather than be ignored.
            Some("-P" | "--physical") => {}
            Some("-L" | "--logical") => {
                return ExitCode::from(usage_error(
                    program,
                    "logical resolution (-L) is not supported",
                ));
            }
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => paths.push(PathBuf::from(arg.as_ref())),
        }
    }
    paths.extend(trailing.iter().map(|arg| PathBuf::from(arg.as_ref())));
    if paths.is_empty() {
        return ExitCode::from(usage_error(program, "a path operand is required"));
    }
    let mut failed = false;
    let mut stdout = std::io::stdout().lock();
    for path in paths {
        match pathutil::resolve(&path, allow_missing) {
            Ok(resolved) => {
                // A resolved path is bytes, not text; printing it through
                // `display()` would substitute U+FFFD for a non-UTF-8 name.
                let bytes = resolved.as_os_str().as_bytes();
                let result = stdout.write_all(bytes).and_then(|()| {
                    if zero {
                        stdout.write_all(b"\0")
                    } else {
                        stdout.write_all(b"\n")
                    }
                });
                if let Err(error) = result {
                    if !quiet {
                        eprintln!("{program}: {error}");
                    }
                    failed = true;
                }
            }
            Err(error) => {
                if !quiet {
                    eprintln!("{program}: {}: {error}", path.display());
                }
                failed = true;
            }
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "realpath",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
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
            run(&["-L".to_string(), ".".to_string()], "realpath"),
            ExitCode::from(rinit_tools::EXIT_USAGE),
            "an unimplemented option must be rejected, not silently ignored"
        );
        assert_eq!(
            run(&["-P".to_string(), ".".to_string()], "realpath"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&[] as &[String], "realpath"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
