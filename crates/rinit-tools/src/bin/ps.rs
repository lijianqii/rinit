//! `ps` - a small process table from `/proc`.

use std::collections::BTreeMap;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, usage_error};

fn passwd() -> BTreeMap<u32, String> {
    let mut users = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string("/etc/passwd") else {
        return users;
    };
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 3 {
            continue;
        }
        if let Ok(uid) = fields[2].parse::<u32>() {
            users.insert(uid, fields[0].to_string());
        }
    }
    users
}

fn stat(pid: &str) -> Option<(String, u64, u64, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let rest = text
        .get(close + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let utime = rest.get(11)?.parse::<u64>().ok()?;
    let stime = rest.get(12)?.parse::<u64>().ok()?;
    let ppid = rest.get(1)?.parse::<u64>().ok()?;
    Some((comm, utime + stime, ppid, uid(pid)))
}

fn uid(pid: &str) -> u64 {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:")
            && let Some(value) = rest.split_whitespace().next()
        {
            return value.parse().unwrap_or(0);
        }
    }
    0
}

fn cmdline(pid: &str, comm: &str) -> String {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return format!("[{comm}]");
    };
    if bytes.is_empty() {
        return format!("[{comm}]");
    }
    let mut parts = Vec::new();
    for field in bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
    {
        parts.push(String::from_utf8_lossy(field).into_owned());
    }
    if parts.is_empty() {
        format!("[{comm}]")
    } else {
        parts.join(" ")
    }
}

fn ticks_time(ticks: u64) -> String {
    // SAFETY: `sysconf` has no pointer arguments; a failure returns -1 and the
    // fallback keeps the output usable.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = if hz > 0 { hz as u64 } else { 100 };
    let seconds = ticks / hz;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    if hours > 0 {
        format!("{hours}:{:02}", minutes % 60)
    } else {
        format!("{minutes}:{:02}", seconds % 60)
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "ps");
    let mut full = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-e" | "-A" => {}
            "-f" => full = true,
            "-h" | "--help" => {
                println!("Usage: ps [-e] [-f]");
                return ExitCode::from(EXIT_OK);
            }
            other => {
                return ExitCode::from(usage_error(&program, &format!("unknown option '{other}'")));
            }
        }
    }

    let users = passwd();
    let mut entries = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        eprintln!("{program}: cannot read /proc");
        return ExitCode::from(1);
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|name| name.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Some((comm, ticks, ppid, uid)) = stat(pid) else {
            continue;
        };
        let user = users
            .get(&(uid as u32))
            .cloned()
            .unwrap_or_else(|| uid.to_string());
        entries.push((
            pid.parse::<u64>().unwrap_or(0),
            user,
            ticks,
            ppid,
            cmdline(pid, &comm),
        ));
    }
    entries.sort_by_key(|entry| entry.0);
    if full {
        println!("UID        PID  PPID     TIME COMMAND");
    } else {
        println!("PID   USER     TIME  COMMAND");
    }
    for (pid, user, ticks, ppid, command) in entries {
        if full {
            println!(
                "{user:<10} {pid:>5} {ppid:>5} {:>8} {command}",
                ticks_time(ticks)
            );
        } else {
            println!("{pid:>5} {user:<8} {:>5} {command}", ticks_time(ticks));
        }
    }
    ExitCode::from(EXIT_OK)
}
