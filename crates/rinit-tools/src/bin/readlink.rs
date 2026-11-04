//! `readlink` - print symbolic link targets or resolved paths.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

use rinit_tools::pathutil;
use rinit_tools::{EXIT_OK, program_name, usage_error};

enum Mode {
    /// Print the link target itself.
    Target,
    /// Every component must exist (`-e`).
    Existing,
    /// All but the final component must exist (`-f`).
    Final,
    /// Missing components are appended lexically (`-m`).
    Missing,
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut mode = Mode::Target;
    let mut newline = true;
    let mut quiet = false;
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
                println!("Usage: readlink [-fenmq] PATH...");
                return ExitCode::from(EXIT_OK);
            }
            Some("-f" | "--canonicalize") => mode = Mode::Final,
            Some("-e" | "--canonicalize-existing") => mode = Mode::Existing,
            Some("-m" | "--canonicalize-missing") => mode = Mode::Missing,
            Some("-n" | "--no-newline") => newline = false,
            Some("-q" | "--quiet") => quiet = true,
            Some("-s" | "--silent") => quiet = true,
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
        let result = match mode {
            Mode::Target => fs::read_link(&path),
            Mode::Existing => pathutil::resolve(&path, false),
            Mode::Final => pathutil::resolve_final(&path),
            Mode::Missing => pathutil::resolve(&path, true),
        };
        match result {
            Ok(target) => {
                // Print the raw bytes: a link target is not required to be
                // UTF-8, and `display()` would replace those bytes with U+FFFD
                // and print a path that does not exist.
                let bytes = target.as_os_str().as_bytes();
                let write = stdout.write_all(bytes).and_then(|()| {
                    if newline {
                        stdout.write_all(b"\n")
                    } else {
                        Ok(())
                    }
                });
                if let Err(error) = write {
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
        "readlink",
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

    /// `-f` requires all but the last component to exist; `-m` does not. They
    /// used to be the same mode, so a script checking for a missing file got a
    /// success it should not have.
    #[test]
    fn canonicalize_modes_differ_on_missing_components() {
        let root = temp_root("modes");
        let missing = root.join("missing");
        let deep = root.join("missing/child");
        assert_eq!(
            run(
                &["-f".to_string(), missing.display().to_string()],
                "readlink"
            ),
            ExitCode::from(EXIT_OK),
            "the final component may be missing"
        );
        assert_eq!(
            run(&["-f".to_string(), deep.display().to_string()], "readlink"),
            ExitCode::from(1),
            "an intermediate component may not be missing"
        );
        assert_eq!(
            run(&["-m".to_string(), deep.display().to_string()], "readlink"),
            ExitCode::from(EXIT_OK)
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
            run(&[] as &[String], "readlink"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
