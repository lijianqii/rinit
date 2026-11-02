//! `uptime` - system uptime and load average.

use std::io;
use std::path::Path;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, read_trimmed, usage_error};

fn parse_uptime(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse::<f64>().ok()
}

fn parse_loadavg(text: &str) -> Option<String> {
    let fields: Vec<&str> = text.split_whitespace().take(3).collect();
    (fields.len() == 3).then(|| fields.join(" "))
}

fn read_uptime() -> io::Result<f64> {
    let text = read_trimmed(Path::new("/proc/uptime"))?;
    parse_uptime(&text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad /proc/uptime"))
}

fn loadavg() -> io::Result<String> {
    let text = read_trimmed(Path::new("/proc/loadavg"))?;
    parse_loadavg(&text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad /proc/loadavg"))
}

fn wall_clock() -> String {
    // SAFETY: `time` and `localtime_r` are given valid, local storage; the
    // returned pointer is only read while `tm` is alive.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    let ok = unsafe { libc::localtime_r(&now, &mut tm) };
    if ok.is_null() {
        return "??:??:??".to_string();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn clock(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        format!(
            "{days} day{}, {:02}:{:02}",
            if days == 1 { "" } else { "s" },
            hours,
            minutes
        )
    } else {
        format!("{:02}:{:02}", hours, minutes)
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "uptime");
    let mut pretty = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Usage: uptime [-p]");
                return ExitCode::from(EXIT_OK);
            }
            "-p" | "--pretty" => pretty = true,
            other => {
                return ExitCode::from(usage_error(&program, &format!("unknown option '{other}'")));
            }
        }
    }
    let seconds = match read_uptime() {
        Ok(value) => value as u64,
        Err(e) => {
            eprintln!("{program}: cannot read uptime: {e}");
            return ExitCode::from(1);
        }
    };
    if pretty {
        println!("up {}", clock(seconds));
        return ExitCode::from(EXIT_OK);
    }
    let loads = match loadavg() {
        Ok(value) => value,
        Err(e) => {
            eprintln!("{program}: cannot read load average: {e}");
            return ExitCode::from(1);
        }
    };
    println!(
        " {} up {}, 0 users, load average: {}",
        wall_clock(),
        clock(seconds),
        loads
    );
    ExitCode::from(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_clock_handles_days_and_hours() {
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(61), "00:01");
        assert_eq!(clock(3661), "01:01");
        assert_eq!(clock(86400), "1 day, 00:00");
        assert_eq!(clock(90061), "1 day, 01:01");
    }

    #[test]
    fn proc_parsers_reject_truncated_data() {
        assert_eq!(parse_uptime("123.5 456.0\n"), Some(123.5));
        assert_eq!(parse_uptime("not-a-number"), None);
        assert_eq!(
            parse_loadavg("0.10 0.20 0.30 1/100 123\n").as_deref(),
            Some("0.10 0.20 0.30")
        );
        assert_eq!(parse_loadavg("0.10 0.20"), None);
    }
}
