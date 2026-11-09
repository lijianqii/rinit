//! `mkdir` - create directories.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
    parents: bool,
    mode: Option<u32>,
}

fn parse_mode(value: &str) -> Result<u32, String> {
    if value.is_empty() {
        return Err("empty mode".to_string());
    }
    let mode =
        u32::from_str_radix(value, 8).map_err(|_| format!("invalid octal mode '{value}'"))?;
    if mode > 0o7777 {
        return Err(format!("mode '{value}' is larger than 07777"));
    }
    Ok(mode)
}

fn make_directory(path: &Path, options: Options) -> Result<(), String> {
    let mut created = true;
    if options.parents {
        if let Some(_name) = path.file_name() {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", path.display()))?;
        }
        match fs::create_dir(path) {
            Ok(()) => {}
            // `-p` succeeds on an existing directory, but must not apply `-m`
            // to it (and must not follow a symlink to chmod its target).
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => created = false,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    } else {
        fs::create_dir(path).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    if let Some(mode) = options.mode
        && created
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut paths: Vec<PathBuf> = Vec::new();
    let marker = args.iter().position(|arg| arg.as_ref() == OsStr::new("--"));
    let (options_args, trailing): (&[A], &[A]) = match marker {
        Some(index) => (
            args.get(..index).unwrap_or(&[]),
            args.get(index + 1..).unwrap_or(&[]),
        ),
        None => (args, &[] as &[A]),
    };
    let mut index = 0;
    while let Some(arg) = options_args.get(index) {
        match arg.as_ref().to_str() {
            Some("--help") => {
                println!("Usage: mkdir [-p] [-m MODE] DIRECTORY...");
                return ExitCode::from(EXIT_OK);
            }
            Some("-p" | "--parents") => options.parents = true,
            Some("-m" | "--mode") => {
                index += 1;
                let Some(value) = options_args.get(index) else {
                    return ExitCode::from(usage_error(program, "-m needs a mode"));
                };
                let Some(value) = value.as_ref().to_str() else {
                    return ExitCode::from(usage_error(program, "-m needs a UTF-8 mode"));
                };
                match parse_mode(value) {
                    Ok(mode) => options.mode = Some(mode),
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => paths.push(PathBuf::from(arg.as_ref())),
        }
        index += 1;
    }
    paths.extend(trailing.iter().map(|arg| PathBuf::from(arg.as_ref())));
    if paths.is_empty() {
        return ExitCode::from(usage_error(program, "a directory operand is required"));
    }
    let mut failed = false;
    for display in paths {
        if let Err(error) = make_directory(&display, options) {
            eprintln!("{program}: {error}");
            failed = true;
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
        "mkdir",
    );
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-mkdir-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn creates_a_directory_and_parents() {
        let root = temp_root("create");
        let single = root.join("single");
        make_directory(&single, Options::default()).expect("single");
        assert!(single.is_dir());
        let nested = root.join("a/b/c");
        assert!(make_directory(&nested, Options::default()).is_err());
        let options = Options {
            parents: true,
            ..Options::default()
        };
        make_directory(&nested, options).expect("nested");
        assert!(nested.is_dir());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mode_is_applied_exactly() {
        let root = temp_root("mode");
        let dir = root.join("private");
        let options = Options {
            mode: Some(0o700),
            ..Options::default()
        };
        make_directory(&dir, options).expect("private");
        assert_eq!(fs::metadata(&dir).expect("meta").mode() & 0o777, 0o700);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parents_does_not_reapply_mode_to_existing_paths() {
        let root = temp_root("existing-mode");
        let dir = root.join("dir");
        fs::create_dir(&dir).expect("dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("mode");
        let options = Options {
            parents: true,
            mode: Some(0o700),
        };
        make_directory(&dir, options).expect("existing");
        assert_eq!(fs::metadata(&dir).expect("meta").mode() & 0o777, 0o755);

        let target = root.join("target");
        fs::create_dir(&target).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("target mode");
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        make_directory(&link, options).expect("symlink");
        assert_eq!(fs::metadata(&target).expect("meta").mode() & 0o777, 0o755);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn mode_parser_rejects_bad_values() {
        assert_eq!(parse_mode("755"), Ok(0o755));
        assert!(parse_mode("").is_err());
        assert!(parse_mode("8").is_err());
        assert!(parse_mode("10000").is_err());
    }

    #[test]
    fn run_reports_usage_and_existing_directories() {
        let root = temp_root("run");
        let dir = root.join("dir");
        assert_eq!(
            run(&[dir.display().to_string()], "mkdir"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&[dir.display().to_string()], "mkdir"),
            ExitCode::from(1)
        );
        assert_eq!(
            run(&["--help".to_string()], "mkdir"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "mkdir"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&[] as &[String], "mkdir"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        let _ = fs::remove_dir_all(root);
    }
}
