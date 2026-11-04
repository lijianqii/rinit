//! `dirname` - strip the last component from a path.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

fn dirname_os(path: &OsStr) -> OsString {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return OsString::from(".");
    }
    let mut end = bytes.len();
    while end > 1 && bytes.get(end - 1) == Some(&b'/') {
        end -= 1;
    }
    let trimmed = bytes.get(..end).unwrap_or(bytes);
    match trimmed.iter().rposition(|byte| *byte == b'/') {
        None => OsString::from("."),
        Some(0) => OsString::from("/"),
        Some(position) => {
            let mut directory = trimmed.get(..position).unwrap_or_default();
            while directory.len() > 1 && directory.last() == Some(&b'/') {
                directory = directory.get(..directory.len() - 1).unwrap_or_default();
            }
            if directory.is_empty() {
                OsString::from("/")
            } else {
                OsString::from_vec(directory.to_vec())
            }
        }
    }
}

#[cfg(test)]
fn dirname(path: &str) -> String {
    dirname_os(OsStr::new(path))
        .into_string()
        .unwrap_or_default()
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut paths: Vec<OsString> = Vec::new();
    for arg in args {
        match arg.as_ref().to_str() {
            Some("--help") => {
                println!("Usage: dirname PATH...");
                return ExitCode::from(EXIT_OK);
            }
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => paths.push(OsString::from(arg.as_ref())),
        }
    }
    if paths.is_empty() {
        return ExitCode::from(usage_error(program, "a path operand is required"));
    }
    let mut out = std::io::stdout().lock();
    for path in paths {
        let result = dirname_os(&path);
        if let Err(error) = out
            .write_all(result.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
        {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return ExitCode::from(EXIT_OK);
            }
            eprintln!("{program}: {error}");
            return ExitCode::from(1);
        }
    }
    ExitCode::from(EXIT_OK)
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "dirname",
    );
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
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
    fn a_non_utf8_path_is_preserved() {
        use std::os::unix::ffi::OsStrExt;
        let path = OsStr::from_bytes(b"/tmp/\xff/file");
        assert_eq!(dirname_os(path).as_bytes(), b"/tmp/\xff");
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
        assert_eq!(
            run(&[] as &[String], "dirname"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
