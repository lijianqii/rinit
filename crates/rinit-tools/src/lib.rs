//! Shared pieces for the independent `rinit-tools` binaries.
//!
//! Every tool is its own ELF and links this library statically. There is no
//! multi-call dispatch: `argv[0]` is not consulted, and an installed file is
//! the program. Keeping the helpers here avoids copy-pasted argument and
//! `/proc` handling without turning the tools into one binary.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]

use std::io;
use std::path::Path;

pub mod control;

/// Exit code for a successful invocation.
pub const EXIT_OK: u8 = 0;
/// Exit code for a general failure.
pub const EXIT_FAILURE: u8 = 1;
/// Exit code for a command-line usage error.
pub const EXIT_USAGE: u8 = 2;

/// The program name from `argv[0]`, without its directory.
///
/// `None` is returned for a missing or non-UTF-8 `argv[0]`; callers use a
/// fixed fallback rather than making a diagnostic depend on an odd byte.
pub fn program_name(argv0: Option<&str>, fallback: &'static str) -> String {
    let Some(argv0) = argv0 else {
        return fallback.to_string();
    };
    match Path::new(argv0).file_name().and_then(|name| name.to_str()) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => fallback.to_string(),
    }
}

/// Parse an unsigned decimal integer, naming the failing argument.
pub fn parse_u64(value: &str, what: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{what}: expected a non-negative integer, got '{value}'"))
}

/// Read a small text file and strip one trailing newline.
///
/// `/proc` files are line-oriented and may end with a newline; keeping that
/// detail out of each tool makes the call sites easier to audit.
pub fn read_trimmed(path: &Path) -> io::Result<String> {
    let text = std::fs::read_to_string(path)?;
    Ok(text.trim_end_matches(['\n', '\r']).to_string())
}

/// Print a usage error and return the documented exit code.
pub fn usage_error(program: &str, message: &str) -> u8 {
    eprintln!("{program}: {message}");
    EXIT_USAGE
}

/// Restore the default `SIGPIPE` disposition at the start of a CLI tool.
///
/// Rust ignores `SIGPIPE`, so `ps | head` would otherwise panic on the first
/// write to a closed pipe. The C tools these replace simply die from the
/// signal; restoring the default preserves that shell-friendly behavior.
pub fn restore_sigpipe() {
    // SAFETY: `signal` is a process-wide disposition change with a valid,
    // constant handler.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Print a general error and return the documented exit code.
pub fn failure(program: &str, message: &str) -> u8 {
    eprintln!("{program}: {message}");
    EXIT_FAILURE
}

/// Split a NUL-separated `key=value` record, as used by `/proc` and mountinfo.
pub fn split_nul(record: &[u8]) -> Vec<&[u8]> {
    record
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_name_drops_the_directory() {
        assert_eq!(program_name(Some("/bin/hostname"), "hostname"), "hostname");
        assert_eq!(program_name(Some("mount"), "mount"), "mount");
        assert_eq!(program_name(None, "free"), "free");
        assert_eq!(program_name(Some("/"), "df"), "df");
    }

    #[test]
    fn unsigned_parser_names_bad_input() {
        assert_eq!(parse_u64("42", "tail"), Ok(42));
        let error = parse_u64("-1", "tail").expect_err("negative is not unsigned");
        assert!(error.contains("tail"), "{error}");
    }

    #[test]
    fn nul_record_splits_and_drops_empty_fields() {
        assert_eq!(
            split_nul(b"one\0two\0\0three\0"),
            vec![&b"one"[..], &b"two"[..], &b"three"[..]]
        );
    }
}
