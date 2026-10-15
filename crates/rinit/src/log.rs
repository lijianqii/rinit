//! Minimal, dependency-free logging for early boot.
//!
//! Writes to stdout (fd 1), which the kernel wires to `/dev/console` for PID 1
//! (plan §2.1⑦). A structured ring-buffer logger arrives in M3; M0 only needs
//! human-visible boot progress.
//!
//! The macro is self-contained: it brings `io::Write` into scope internally so
//! callers do not need any import.

macro_rules! rlog {
    ($($arg:tt)*) => {{
        let line = format!($($arg)*);
        {
            let mut out = std::io::stdout();
            let _ = std::io::Write::write_fmt(&mut out, format_args!("rinit: {line}\n"));
            let _ = std::io::Write::flush(&mut out);
        }
    }};
}

pub(crate) use rlog;
