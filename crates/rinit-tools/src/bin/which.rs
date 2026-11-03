//! `which` - locate a command in PATH.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

fn executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn find_in_path(name: &str, path: &OsStr, all: bool) -> Vec<PathBuf> {
    if name.contains('/') {
        let path = Path::new(name);
        return if executable(path) {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let mut found = Vec::new();
    for directory in std::env::split_paths(path) {
        let directory = if directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            directory
        };
        let candidate = directory.join(name);
        if executable(&candidate) {
            found.push(candidate);
            if !all {
                break;
            }
        }
    }
    found
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut all = false;
    let mut names: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: which [-a] COMMAND...");
                return ExitCode::from(EXIT_OK);
            }
            "-a" | "--all" => all = true,
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => names.push(value.to_string()),
        }
    }
    if names.is_empty() {
        return ExitCode::from(usage_error(program, "a command name is required"));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut failed = false;
    for name in names {
        let found = find_in_path(&name, &path, all);
        if found.is_empty() {
            failed = true;
        }
        for candidate in found {
            println!("{}", candidate.display());
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "which");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-which-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("root");
        root
    }

    fn path_of(directory: &Path) -> OsString {
        std::env::join_paths([directory]).expect("path")
    }

    #[test]
    fn finds_executables_and_skips_non_executables() {
        let root = temp_root("find");
        let tool = root.join("tool");
        fs::write(&tool, b"#!/bin/sh\n").expect("tool");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).expect("mode");
        let data = root.join("data");
        fs::write(&data, b"x").expect("data");
        let path = path_of(&root);
        assert_eq!(find_in_path("tool", &path, false), vec![root.join("tool")]);
        assert!(find_in_path("data", &path, false).is_empty());
        assert!(find_in_path("missing", &path, false).is_empty());
        assert_eq!(
            find_in_path(&tool.display().to_string(), &path, false),
            vec![tool]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn all_reports_every_match() {
        let first = temp_root("first");
        let second = temp_root("second");
        for directory in [&first, &second] {
            let tool = directory.join("tool");
            fs::write(&tool, b"x").expect("tool");
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).expect("mode");
        }
        let path = std::env::join_paths([&first, &second]).expect("path");
        assert_eq!(find_in_path("tool", &path, false).len(), 1);
        assert_eq!(find_in_path("tool", &path, true).len(), 2);
        let _ = fs::remove_dir_all(first);
        let _ = fs::remove_dir_all(second);
    }

    #[test]
    fn run_handles_help_and_missing_commands() {
        assert_eq!(
            run(&["--help".to_string()], "which"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "which"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
        assert_eq!(
            run(&["definitely-not-a-command".to_string()], "which"),
            ExitCode::from(1)
        );
    }
}
