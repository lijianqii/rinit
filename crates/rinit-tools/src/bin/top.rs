//! `top` - a bounded process snapshot for the device image.
//!
//! This is deliberately a snapshot tool, not a terminal UI: `-b -n 1` is the
//! stable form used by scripts and by the acceptance tests. Repeated output is
//! supported with `-n`; a future version can add interactive keys.

use std::collections::BTreeMap;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, read_trimmed, usage_error};

fn meminfo() -> BTreeMap<String, u64> {
    let mut values = BTreeMap::new();
    let Ok(text) = read_trimmed(std::path::Path::new("/proc/meminfo")) else {
        return values;
    };
    for line in text.lines() {
        if let Some((key, rest)) = line.split_once(':')
            && let Some(value) = rest.split_whitespace().next()
            && let Ok(value) = value.parse::<u64>()
        {
            values.insert(key.to_string(), value);
        }
    }
    values
}

fn loadavg() -> String {
    read_trimmed(std::path::Path::new("/proc/loadavg"))
        .map(|text| {
            text.split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|_| "?".to_string())
}

fn uptime_secs() -> u64 {
    read_trimmed(std::path::Path::new("/proc/uptime"))
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0) as u64
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

fn uptime_text(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = (seconds % 86400) / 3600;
    let minutes = (seconds % 3600) / 60;
    if days > 0 {
        format!("{days} days, {hours:02}:{minutes:02}")
    } else {
        format!("{hours:02}:{minutes:02}")
    }
}

struct Proc {
    pid: u64,
    state: char,
    rss_kb: u64,
    command: String,
}

fn read_proc(pid: &str) -> Option<Proc> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    let rest = stat
        .get(close + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let state = rest.first()?.chars().next()?;
    let mut rss_kb = 0;
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:")
                && let Some(value) = value.split_whitespace().next()
            {
                rss_kb = value.parse().unwrap_or(0);
                break;
            }
        }
    }
    let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let command = if cmd.is_empty() {
        format!("[{comm}]")
    } else {
        cmd.split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .map(|field| String::from_utf8_lossy(field).into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    };
    Some(Proc {
        pid: pid.parse().ok()?,
        state,
        rss_kb,
        command,
    })
}

fn snapshot() -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut processes: Vec<Proc> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let pid = name.to_str()?;
            if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            read_proc(pid)
        })
        .collect();
    processes.sort_by_key(|process| std::cmp::Reverse(process.rss_kb));
    processes
}

fn print_snapshot() {
    let mem = meminfo();
    let total = mem.get("MemTotal").copied().unwrap_or(0);
    let free = mem.get("MemFree").copied().unwrap_or(0);
    let available = mem.get("MemAvailable").copied().unwrap_or(free);
    let processes = snapshot();
    println!(
        "top - {} up {}, load average: {}",
        wall_clock(),
        uptime_text(uptime_secs()),
        loadavg()
    );
    println!(
        "Tasks: {} total    Mem: {} total, {} free, {} available",
        processes.len(),
        human(total),
        human(free),
        human(available)
    );
    println!("  PID STAT   RSS COMMAND");
    for process in processes.iter().take(20) {
        println!(
            "{:>5} {:>4} {:>5} {}",
            process.pid,
            process.state,
            human(process.rss_kb),
            process.command
        );
    }
}

fn human(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.1}G", kib as f64 / (1024.0 * 1024.0))
    } else if kib >= 1024 {
        format!("{:.1}M", kib as f64 / 1024.0)
    } else {
        format!("{kib}K")
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "top");
    let mut iterations = 1u64;
    let mut index = 0;
    let args: Vec<String> = std::env::args().skip(1).collect();
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Usage: top [-b] [-n COUNT]");
                return ExitCode::from(EXIT_OK);
            }
            "-b" => {}
            "-n" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return ExitCode::from(usage_error(&program, "-n needs a count"));
                };
                iterations = match value.parse::<u64>() {
                    Ok(value) if value > 0 => value,
                    _ => return ExitCode::from(usage_error(&program, "-n needs a positive count")),
                };
            }
            other => {
                return ExitCode::from(usage_error(&program, &format!("unknown option '{other}'")));
            }
        }
        index += 1;
    }
    for iteration in 0..iterations {
        if iteration > 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        print_snapshot();
    }
    ExitCode::from(EXIT_OK)
}
