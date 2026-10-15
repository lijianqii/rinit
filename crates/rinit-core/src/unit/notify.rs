//! `sd_notify` message parsing (plan §1.3(12)).
//!
//! A `Type=notify` service tells rinit it is up by sending `KEY=VALUE` lines to
//! the unit's notify socket. rinit implements the minimal compatible subset the
//! plan calls for - `READY=1` and `STATUS=` - and ignores everything else, which
//! is what makes the handshake forward-compatible: a service may send
//! `WATCHDOG=1`, `MAINPID=`, `ERRNO=` or its own keys without rinit rejecting
//! the whole datagram.
//!
//! Parsing lives here, apart from the socket, because it is the part with all
//! the edge cases (blank lines, a value containing `=`, a non-UTF-8 status) and
//! none of the syscalls.

use alloc::string::String;

/// The subset of an `sd_notify` message rinit understands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifyMessage {
    /// `READY=1` was seen: start-up is complete.
    pub ready: bool,
    /// The latest `STATUS=` text, if one was sent. Shown by `rctl status`.
    pub status: Option<String>,
    /// `WATCHDOG=1` was seen.
    ///
    /// M2 records it but takes no action (plan §1.3(12): the watchdog lands
    /// with the hardware watchdog in P2). Keeping the field means the parser
    /// does not have to change when it does.
    pub watchdog: bool,
}

/// Why a notify datagram was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyError {
    /// A non-empty line was not `KEY=VALUE`.
    Malformed {
        /// The 1-based line number within the datagram.
        line: usize,
    },
}

impl core::fmt::Display for NotifyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NotifyError::Malformed { line } => {
                write!(f, "notify line {line} is not KEY=VALUE")
            }
        }
    }
}

/// Parse one datagram received on a notify socket.
///
/// A datagram may carry several newline-separated assignments. Blank lines are
/// ignored, because a trailing newline is normal. A non-empty line without a
/// `=` is reported as an error rather than dropped: silently ignoring it would
/// hide a bug in the client and make a service that never becomes ready
/// impossible to diagnose.
///
/// # Errors
/// [`NotifyError::Malformed`] if a non-empty line has no `=`.
pub fn parse(message: &[u8]) -> Result<NotifyMessage, NotifyError> {
    let mut out = NotifyMessage::default();

    for (index, raw) in message.split(|&b| b == b'\n').enumerate() {
        // A `\r\n` from a client that writes text mode would otherwise leave a
        // stray `\r` on the value.
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            continue;
        }

        let Some(eq) = line.iter().position(|&b| b == b'=') else {
            return Err(NotifyError::Malformed { line: index + 1 });
        };
        // Split on the *first* `=`, so a value may itself contain `=`.
        let key = &line[..eq];
        let value = &line[eq + 1..];

        match key {
            b"READY" => out.ready = value == b"1",
            b"STATUS" => out.status = Some(String::from_utf8_lossy(value).into_owned()),
            b"WATCHDOG" => out.watchdog = value == b"1",
            // Unknown keys are ignored on purpose (see the module docs).
            _ => {}
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;

    fn ok(bytes: &[u8]) -> NotifyMessage {
        parse(bytes).expect("a valid datagram")
    }

    #[test]
    fn ready_one_marks_the_unit_ready() {
        let msg = ok(b"READY=1");
        assert!(msg.ready);
        assert_eq!(msg.status, None);
        assert!(!msg.watchdog);
    }

    /// `READY=0` is not a readiness signal; the plan only defines `READY=1`.
    #[test]
    fn ready_zero_does_not_mark_ready() {
        assert!(!ok(b"READY=0").ready);
        assert!(!ok(b"READY=").ready);
    }

    #[test]
    fn status_is_captured_verbatim() {
        assert_eq!(
            ok(b"STATUS=serving 3 clients").status.as_deref(),
            Some("serving 3 clients")
        );
    }

    /// A value containing `=` must keep everything after the first one.
    #[test]
    fn status_may_contain_equals_signs() {
        assert_eq!(ok(b"STATUS=a=b=c").status.as_deref(), Some("a=b=c"));
    }

    /// An explicit empty status is distinct from no status at all.
    #[test]
    fn empty_status_is_some_empty_string() {
        assert_eq!(ok(b"STATUS=").status.as_deref(), Some(""));
    }

    #[test]
    fn several_lines_are_all_applied() {
        let msg = ok(b"STATUS=booting\nREADY=1\n");
        assert!(msg.ready);
        assert_eq!(msg.status.as_deref(), Some("booting"));
    }

    #[test]
    fn blank_lines_are_ignored() {
        let msg = ok(b"\n\nREADY=1\n\n");
        assert!(msg.ready);
    }

    #[test]
    fn crlf_line_endings_are_accepted() {
        assert_eq!(
            ok(b"STATUS=up\r\nREADY=1\r\n").status.as_deref(),
            Some("up")
        );
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let msg = ok(b"MAINPID=42\nERRNO=0\nREADY=1\n");
        assert!(msg.ready, "an unknown key must not break the datagram");
    }

    #[test]
    fn watchdog_is_parsed_but_not_acted_on() {
        assert!(ok(b"WATCHDOG=1").watchdog);
        assert!(!ok(b"WATCHDOG=0").watchdog);
    }

    /// A last-writer-wins rule keeps a status update followed by another one
    /// from silently keeping the first.
    #[test]
    fn later_status_wins() {
        assert_eq!(
            ok(b"STATUS=first\nSTATUS=second").status.as_deref(),
            Some("second")
        );
    }

    #[test]
    fn a_non_utf8_status_is_lossy_not_fatal() {
        let msg = ok(b"STATUS=\xff\xfe");
        assert!(
            msg.status.is_some(),
            "invalid UTF-8 must not drop the message"
        );
    }

    #[test]
    fn a_line_without_equals_is_rejected() {
        assert_eq!(parse(b"READY"), Err(NotifyError::Malformed { line: 1 }));
        assert_eq!(
            parse(b"READY=1\nbroken"),
            Err(NotifyError::Malformed { line: 2 })
        );
    }

    #[test]
    fn the_error_names_the_offending_line() {
        let err = parse(b"READY=1\n\nnope").unwrap_err();
        assert_eq!(err, NotifyError::Malformed { line: 3 });
        assert!(err.to_string().contains("line 3"), "got: {err}");
    }

    #[test]
    fn an_empty_datagram_yields_nothing() {
        assert_eq!(ok(b""), NotifyMessage::default());
    }
}
