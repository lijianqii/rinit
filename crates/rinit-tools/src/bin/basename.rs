//! `basename` - strip directory and suffix from a path.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

fn basename_os(path: &OsStr, suffix: Option<&OsStr>) -> OsString {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return OsString::new();
    }
    let mut end = bytes.len();
    while end > 1 && bytes.get(end - 1) == Some(&b'/') {
        end -= 1;
    }
    let trimmed = bytes.get(..end).unwrap_or(bytes);
    let mut name = if trimmed == b"/" {
        trimmed
    } else {
        match trimmed.iter().rposition(|byte| *byte == b'/') {
            Some(position) => trimmed.get(position + 1..).unwrap_or(trimmed),
            None => trimmed,
        }
    };
    if let Some(suffix) = suffix {
        let suffix = suffix.as_bytes();
        if !suffix.is_empty()
            && name != suffix
            && let Some(stripped) = name.strip_suffix(suffix)
        {
            name = stripped;
        }
    }
    OsString::from_vec(name.to_vec())
}

#[cfg(test)]
fn basename(path: &str, suffix: Option<&str>) -> String {
    basename_os(OsStr::new(path), suffix.map(OsStr::new))
        .into_string()
        .unwrap_or_default()
}

fn run<A: AsRef<OsStr>>(args: &[A], program: &str) -> ExitCode {
    let mut operands: Vec<OsString> = Vec::new();
    let mut parsing_options = true;
    for arg in args {
        if !parsing_options {
            operands.push(OsString::from(arg.as_ref()));
            continue;
        }
        match arg.as_ref().to_str() {
            Some("--") => parsing_options = false,
            Some("--help") => {
                println!("Usage: basename NAME [SUFFIX]");
                return ExitCode::from(EXIT_OK);
            }
            Some(value) if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => operands.push(OsString::from(arg.as_ref())),
        }
    }
    let (name, suffix) = match operands.as_slice() {
        [name] => (name.as_os_str(), None),
        [name, suffix] => (name.as_os_str(), Some(suffix.as_os_str())),
        _ => {
            return ExitCode::from(usage_error(
                program,
                "NAME and optional SUFFIX are required",
            ));
        }
    };
    let result = basename_os(name, suffix);
    let mut out = std::io::stdout().lock();
    let write = out
        .write_all(result.as_bytes())
        .and_then(|()| out.write_all(b"\n"));
    match write {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::from(EXIT_OK),
        Err(error) => {
            eprintln!("{program}: {error}");
            ExitCode::from(1)
        }
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "basename",
    );
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
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
    fn a_non_utf8_component_is_preserved() {
        use std::os::unix::ffi::OsStrExt;
        let name = OsStr::from_bytes(b"dir/\xff");
        assert_eq!(basename_os(name, None).as_bytes(), b"\xff");
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
            run(&[] as &[String], "basename"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
