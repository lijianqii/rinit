//! `basename` - strip directory and suffix from a path.

use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

fn basename(path: &str, suffix: Option<&str>) -> String {
    if path.is_empty() {
        return String::new();
    }
    let trimmed = path.trim_end_matches('/');
    let name = if trimmed.is_empty() {
        "/"
    } else {
        trimmed.rsplit('/').next().unwrap_or(trimmed)
    };
    if let Some(suffix) = suffix
        && !suffix.is_empty()
        && name != suffix
        && let Some(stripped) = name.strip_suffix(suffix)
    {
        return stripped.to_string();
    }
    name.to_string()
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut operands: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: basename NAME [SUFFIX]");
                return ExitCode::from(EXIT_OK);
            }
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => operands.push(value.to_string()),
        }
    }
    let (name, suffix) = match operands.as_slice() {
        [name] => (name.as_str(), None),
        [name, suffix] => (name.as_str(), Some(suffix.as_str())),
        _ => {
            return ExitCode::from(usage_error(
                program,
                "NAME and optional SUFFIX are required",
            ));
        }
    };
    println!("{}", basename(name, suffix));
    ExitCode::from(EXIT_OK)
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "basename");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_directories_and_suffixes() {
        assert_eq!(basename("foo", None), "foo");
        assert_eq!(basename("foo/bar", None), "bar");
        assert_eq!(basename("foo/bar/", None), "bar");
        assert_eq!(basename("/", None), "/");
        assert_eq!(basename("///", None), "/");
        assert_eq!(basename("", None), "");
        assert_eq!(basename("file.txt", Some(".txt")), "file");
        assert_eq!(basename("file.txt", Some(".rs")), "file.txt");
        assert_eq!(basename("txt", Some("txt")), "txt");
    }

    #[test]
    fn run_handles_help_and_operands() {
        assert_eq!(
            run(&["--help".to_string()], "basename"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["/a/b".to_string()], "basename"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["a".to_string(), "b".to_string()], "basename"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&[], "basename"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
