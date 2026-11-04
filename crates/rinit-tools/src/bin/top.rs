//! `top` - a bounded process snapshot for the device image.
//!
//! This is deliberately a snapshot tool, not a terminal UI: `-b -n 1` is the
//! stable form used by scripts and by the acceptance tests. Repeated output is
//! supported with `-n`; a future version can add interactive keys.

use std::collections::BTreeMap;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, read_trimmed, usage_error};

fn meminfo(root: &std::path::Path) -> BTreeMap<String, u64> {
    let mut values = BTreeMap::new();
    let Ok(text) = read_trimmed(&root.join("proc/meminfo")) else {
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

fn loadavg(root: &std::path::Path) -> String {
    read_trimmed(&root.join("proc/loadavg"))
        .map(|text| {
            text.split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|_| "?".to_string())
}

fn uptime_secs(root: &std::path::Path) -> u64 {
    read_trimmed(&root.join("proc/uptime"))
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

fn read_proc(pid: &str, root: &std::path::Path) -> Option<Proc> {
    let stat = std::fs::read_to_string(root.join(format!("proc/{pid}/stat"))).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    let rest = stat
        .get(close + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    let state = rest.first()?.chars().next()?;
    let mut rss_kb = 0;
    if let Ok(status) = std::fs::read_to_string(root.join(format!("proc/{pid}/status"))) {
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:")
                && let Some(value) = value.split_whitespace().next()
            {
                rss_kb = value.parse().unwrap_or(0);
                break;
            }
        }
    }
    let cmd = std::fs::read(root.join(format!("proc/{pid}/cmdline"))).unwrap_or_default();
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

fn snapshot(root: &std::path::Path) -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir(root.join("proc")) else {
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
            read_proc(pid, root)
        })
        .collect();
    processes.sort_by_key(|process| std::cmp::Reverse(process.rss_kb));
    processes
}

fn print_snapshot(root: &std::path::Path) {
    let mem = meminfo(root);
    let total = mem.get("MemTotal").copied().unwrap_or(0);
    let free = mem.get("MemFree").copied().unwrap_or(0);
    let available = mem.get("MemAvailable").copied().unwrap_or(free);
    let processes = snapshot(root);
    println!(
        "top - {} up {}, load average: {}",
        wall_clock(),
        uptime_text(uptime_secs(root)),
        loadavg(root)
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

fn run(args: &[String], root: &std::path::Path, program: &str) -> ExitCode {
    let mut iterations = 1u64;
    let mut index = 0;
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
                    return ExitCode::from(usage_error(program, "-n needs a count"));
                };
                iterations = match value.parse::<u64>() {
                    Ok(value) if value > 0 => value,
                    _ => return ExitCode::from(usage_error(program, "-n needs a positive count")),
                };
            }
            other => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{other}'")));
            }
        }
        index += 1;
    }
    for iteration in 0..iterations {
        if iteration > 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        print_snapshot(root);
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
        "top",
    );
    let Some(args) = rinit_tools::utf8_args(&program) else {
        return ExitCode::from(rinit_tools::EXIT_USAGE);
    };
    let args: Vec<String> = args.into_iter().skip(1).collect();
    run(&args, std::path::Path::new("/"), &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_and_uptime_formatting() {
        assert_eq!(human(512), "512K");
        assert_eq!(human(2048), "2.0M");
        assert_eq!(uptime_text(61), "00:01");
        assert_eq!(uptime_text(90061), "1 days, 01:01");
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;

    fn test_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-top-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc/1")).expect("proc");
        std::fs::write(
            root.join("proc/meminfo"),
            "MemTotal: 2048 kB\nMemFree: 1024 kB\nMemAvailable: 1536 kB\n",
        )
        .expect("meminfo");
        std::fs::write(root.join("proc/loadavg"), "0.1 0.2 0.3 1/10 42\n").expect("loadavg");
        std::fs::write(root.join("proc/uptime"), "3661.0 0.0\n").expect("uptime");
        std::fs::write(
            root.join("proc/1/stat"),
            "1 (init) S 0 1 1 0 -1 0 0 0 0 0 10 20 0 0\n",
        )
        .expect("stat");
        std::fs::write(root.join("proc/1/status"), "VmRSS:\t1024 kB\n").expect("status");
        std::fs::write(root.join("proc/1/cmdline"), "/sbin/rinit\0").expect("cmdline");
        root
    }

    #[test]
    fn run_prints_a_batch_snapshot() {
        let root = test_root("run");
        assert_eq!(
            run(
                &["-b".to_string(), "-n".to_string(), "1".to_string()],
                &root,
                "top"
            ),
            ExitCode::from(0)
        );
        assert_eq!(
            run(&["-n".to_string(), "2".to_string()], &root, "top"),
            ExitCode::from(0)
        );
        assert_eq!(
            run(&["--help".to_string()], &root, "top"),
            ExitCode::from(0)
        );
        assert_eq!(
            run(&["-n".to_string(), "0".to_string()], &root, "top"),
            ExitCode::from(2)
        );
        assert_eq!(run(&["--bad".to_string()], &root, "top"), ExitCode::from(2));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn empty_proc_still_renders() {
        let root = std::env::temp_dir().join(format!("rinit-top-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc")).expect("proc");
        assert_eq!(run(&["-b".to_string()], &root, "top"), ExitCode::from(0));
        let _ = std::fs::remove_dir_all(root);
    }
}
