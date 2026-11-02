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

fn read_uptime(root: &Path) -> io::Result<f64> {
    let text = read_trimmed(&root.join("proc/uptime"))?;
    parse_uptime(&text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad /proc/uptime"))
}

fn loadavg(root: &Path) -> io::Result<String> {
    let text = read_trimmed(&root.join("proc/loadavg"))?;
    parse_loadavg(&text)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad /proc/loadavg"))
}

fn wall_clock() -> String {
    // SAFETY: `time` and `localtime_r` use valid local storage for the call.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
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

fn run(args: &[String], root: &Path, program: &str) -> ExitCode {
    let mut pretty = false;
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Usage: uptime [-p]");
                return ExitCode::from(EXIT_OK);
            }
            "-p" | "--pretty" => pretty = true,
            other => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{other}'")));
            }
        }
    }
    let seconds = match read_uptime(root) {
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
    let loads = match loadavg(root) {
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

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "uptime");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, Path::new("/"), &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-uptime-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc")).expect("root");
        root
    }

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

    #[test]
    fn run_reads_a_fake_proc() {
        let root = test_root("run");
        std::fs::write(root.join("proc/uptime"), "3661.5 0.0\n").expect("uptime");
        std::fs::write(root.join("proc/loadavg"), "0.1 0.2 0.3 1/10 42\n").expect("loadavg");
        assert_eq!(run(&[], &root, "uptime"), ExitCode::from(0));
        assert_eq!(run(&["-p".to_string()], &root, "uptime"), ExitCode::from(0));
        assert_eq!(
            run(&["--help".to_string()], &root, "uptime"),
            ExitCode::from(0)
        );
        assert_eq!(
            run(&["--bad".to_string()], &root, "uptime"),
            ExitCode::from(2)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_proc_is_a_failure() {
        let root = test_root("missing");
        assert_eq!(run(&[], &root, "uptime"), ExitCode::from(1));
        let _ = std::fs::remove_dir_all(root);
    }
}
