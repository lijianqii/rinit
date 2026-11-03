//! `dirname` - strip the last component from a path.

use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

fn dirname(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let mut end = path.len();
    while end > 1 && path.as_bytes().get(end - 1) == Some(&b'/') {
        end -= 1;
    }
    let trimmed = &path[..end];
    match trimmed.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(position) => {
            let mut directory = &trimmed[..position];
            while directory.len() > 1 && directory.ends_with('/') {
                directory = &directory[..directory.len() - 1];
            }
            if directory.is_empty() {
                "/".to_string()
            } else {
                directory.to_string()
            }
        }
    }
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut paths: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: dirname PATH...");
                return ExitCode::from(EXIT_OK);
            }
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            value => paths.push(value.to_string()),
        }
    }
    if paths.is_empty() {
        return ExitCode::from(usage_error(program, "a path operand is required"));
    }
    for path in paths {
        println!("{}", dirname(&path));
    }
    ExitCode::from(EXIT_OK)
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "dirname");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_last_component() {
        assert_eq!(dirname("foo"), ".");
        assert_eq!(dirname("foo/bar"), "foo");
        assert_eq!(dirname("foo/bar/"), "foo");
        assert_eq!(dirname("/foo"), "/");
        assert_eq!(dirname("/foo/bar"), "/foo");
        assert_eq!(dirname("/"), "/");
        assert_eq!(dirname("///"), "/");
        assert_eq!(dirname(""), ".");
    }

    #[test]
    fn run_handles_help_and_missing_operands() {
        assert_eq!(
            run(&["--help".to_string()], "dirname"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["/a/b".to_string()], "dirname"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(run(&[], "dirname"), ExitCode::from(rinit_tools::EXIT_USAGE));
    }
}
