//! Kernel uptime, for measuring how long the boot took (plan §4.3).
//!
//! The kernel's own clock is the only one that covers the whole boot: rinit starts
//! some way into it, so a number read here can be compared with the timestamp the
//! kernel printed when it handed over (`[...] Run /sbin/rinit as init process`),
//! and the difference is the answer to "内核启动到 rinit 拉首个服务".
//!
//! The parse is pure and separate from the read so it is testable without a
//! `/proc`, and it is integer arithmetic rather than a float: the numbers go into a
//! log line somebody compares by eye, and a float's formatting is a second thing
//! that can be wrong.

/// Parse the first field of `/proc/uptime` into milliseconds.
///
/// The file is `<seconds>.<centiseconds> <idle>`, e.g. `2.94 1.20`. Returns
/// `None` for anything that is not that: a missing or reshaped `/proc/uptime`
/// should cost a measurement, not the boot.
pub fn parse_uptime_ms(text: &str) -> Option<u64> {
    let seconds = text.split_whitespace().next()?;
    let (whole, fraction) = match seconds.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (seconds, ""),
    };
    let whole: u64 = whole.parse().ok()?;
    // Two digits is what the kernel prints; take at most three and pad, so
    // `2.9` is 2900 ms and `2.949` is 2949 ms rather than a truncated 2940.
    let mut digits: Vec<u8> = fraction
        .bytes()
        .take(3)
        .filter(u8::is_ascii_digit)
        .collect();
    while digits.len() < 3 {
        digits.push(b'0');
    }
    let millis: u64 = std::str::from_utf8(&digits).ok()?.parse().ok()?;
    Some(whole.saturating_mul(1000).saturating_add(millis))
}

/// The kernel's uptime in milliseconds, or `None` when `/proc` is not up.
pub fn uptime_ms() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    parse_uptime_ms(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernels_own_format_parses() {
        assert_eq!(parse_uptime_ms("2.94 1.20\n"), Some(2940));
        assert_eq!(parse_uptime_ms("0.00 0.00\n"), Some(0));
        assert_eq!(parse_uptime_ms("1234.5 10.0"), Some(1_234_500));
        // Three digits of fraction are used as printed...
        assert_eq!(parse_uptime_ms("2.949 1.0"), Some(2949));
        // ...and anything beyond them is dropped rather than rounded into a
        // number the kernel did not print.
        assert_eq!(parse_uptime_ms("2.9499 1.0"), Some(2949));
    }

    /// A reshaped or missing file costs the measurement, not the boot.
    #[test]
    fn anything_that_is_not_that_returns_none() {
        assert_eq!(parse_uptime_ms(""), None);
        assert_eq!(parse_uptime_ms("not a number"), None);
        assert_eq!(
            parse_uptime_ms("12"),
            Some(12_000),
            "no fraction means zero"
        );
    }
}
