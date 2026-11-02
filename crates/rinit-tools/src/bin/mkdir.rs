//! `mkdir` - create directories.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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
    let result = if options.parents {
        fs::create_dir_all(path)
    } else {
        fs::create_dir(path)
    };
    result.map_err(|error| format!("{}: {error}", path.display()))?;
    if let Some(mode) = options.mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut options = Options::default();
    let mut paths: Vec<String> = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--help" => {
                println!("Usage: mkdir [-p] [-m MODE] DIRECTORY...");
                return ExitCode::from(EXIT_OK);
            }
            "-p" | "--parents" => options.parents = true,
            "-m" | "--mode" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return ExitCode::from(usage_error(program, "-m needs a mode"));
                };
                match parse_mode(value) {
                    Ok(mode) => options.mode = Some(mode),
                    Err(error) => return ExitCode::from(usage_error(program, &error)),
                }
            }
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => paths.push(value.to_string()),
        }
        index += 1;
    }
    if paths.is_empty() {
        return ExitCode::from(usage_error(program, "a directory operand is required"));
    }
    let mut failed = false;
    for display in paths {
        if let Err(error) = make_directory(Path::new(&display), options) {
            eprintln!("{program}: {error}");
            failed = true;
        }
    }
    ExitCode::from(if failed { 1 } else { EXIT_OK })
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "mkdir");
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        assert_eq!(run(&[], "mkdir"), ExitCode::from(rinit_tools::EXIT_USAGE));
        let _ = fs::remove_dir_all(root);
    }
}
