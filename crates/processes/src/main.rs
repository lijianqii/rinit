//! processes - a small `/proc` process viewer for rinit images.
//!
//! The default view is a flat `ps`-style table. RSS is read for every process;
//! PSS/USS require walking `/proc/<pid>/smaps_rollup`, so they are opt-in with
//! `--long` or `--json`. Parent/child relationships come from the PPID in
//! `/proc/<pid>/stat`.

// Production code must not panic: a bad command line is a usage error, not a
// crash with exit code 101. Tests are exempt.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

// Linux exposes `/proc/<pid>/stat`'s rss in pages and utime/stime in USER_HZ
// ticks. USER_HZ is the userspace ABI value (100) and does not follow
// CONFIG_HZ. All supported rinit targets use 4 KiB pages; a 64 KiB-page kernel
// would need this read from the system rather than hardcoded.
const PAGE_SIZE: u64 = 4096;
const CLOCK_TICKS: u64 = 100;

const USAGE: &str = "\
Usage: processes [OPTIONS]

Show processes from /proc.

Options:
  --tree                 Show the process tree
  --long                 Add PSS, USS and swap columns
  --json                 Emit a JSON array
  --sort=KEY             pid, ppid, cpu, rss, vsz, name (default: pid)
  --reverse              Reverse the sort
  --limit=N              Only show the first N rows (with --tree, a cut can
                          leave a visible parent without its children)
  --pid=LIST             Comma-separated PIDs
  --user=USER            Filter by user name or numeric uid
  --no-header            Omit the table header
  --ascii                ASCII tree connectors
  --full                 Do not truncate commands (currently the default)
  --proc-root=DIR        Read a different /proc-like tree
  -h, --help             Show this help
  -V, --version          Show the version
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Pid,
    Ppid,
    Cpu,
    Rss,
    Vsz,
    Name,
}

#[derive(Debug)]
struct Options {
    proc_root: PathBuf,
    tree: bool,
    long: bool,
    json: bool,
    ascii: bool,
    no_header: bool,
    sort: SortKey,
    reverse: bool,
    limit: Option<usize>,
    pids: Option<BTreeSet<u32>>,
    user: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            proc_root: PathBuf::from("/proc"),
            tree: false,
            long: false,
            json: false,
            ascii: false,
            no_header: false,
            sort: SortKey::Pid,
            reverse: false,
            limit: None,
            pids: None,
            user: None,
        }
    }
}

#[derive(Debug, Clone)]
struct Stat {
    pid: u32,
    ppid: u32,
    comm: String,
    state: char,
    utime: u64,
    stime: u64,
    starttime: u64,
    vsize: u64,
    rss_bytes: u64,
}

#[derive(Debug, Default, Clone)]
struct Status {
    uid: Option<u32>,
    threads: Option<u64>,
    vm_swap: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Smaps {
    pss: Option<u64>,
    uss: Option<u64>,
}

#[derive(Debug, Clone)]
struct Process {
    pid: u32,
    ppid: u32,
    uid: u32,
    user: String,
    state: char,
    vsz: u64,
    rss: u64,
    pss: Option<u64>,
    uss: Option<u64>,
    swap: Option<u64>,
    cpu_usec: u64,
    cpu_percent: f64,
    threads: Option<u64>,
    command: String,
}

fn main() {
    let args = match collect_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(argument) => {
            eprintln!("processes: argument {argument:?} is not valid UTF-8\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let opts = match parse_args(args.into_iter()) {
        Ok(Some(opts)) => opts,
        Ok(None) => {
            print!("{USAGE}");
            return;
        }
        Err(message) => {
            eprintln!("processes: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    match run(&opts) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("processes: {e}");
            std::process::exit(1);
        }
    }
}

fn collect_args(args: impl Iterator<Item = OsString>) -> Result<Vec<String>, OsString> {
    args.map(OsString::into_string).collect()
}

fn run(opts: &Options) -> io::Result<()> {
    let uptime = read_uptime(&opts.proc_root).unwrap_or(0.0);
    let users = load_users();
    let details = opts.long || opts.json;
    let mut processes = read_processes(opts, &users, uptime, details)?;
    filter_processes(&mut processes, opts);

    if opts.tree {
        sort_for_tree(&mut processes);
    } else {
        sort_processes(&mut processes, opts);
    }
    if let Some(limit) = opts.limit {
        processes.truncate(limit);
    }

    if opts.json {
        print_json(&processes);
    } else if opts.tree {
        print_tree(&processes, opts);
    } else {
        print_table(&processes, opts);
    }
    Ok(())
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Options>, String> {
    let mut opts = Options::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-V" | "--version" => {
                println!("processes {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--tree" => opts.tree = true,
            "--long" => opts.long = true,
            "--json" => opts.json = true,
            "--ascii" => opts.ascii = true,
            "--no-header" => opts.no_header = true,
            "--full" => {}
            "--reverse" => opts.reverse = true,
            "--sort" => {
                let value = args.next().ok_or("--sort needs a value")?;
                opts.sort = parse_sort(&value)?;
            }
            "--limit" => {
                let value = args.next().ok_or("--limit needs a value")?;
                opts.limit = Some(parse_number(&value, "--limit")?);
            }
            "--pid" => {
                let value = args.next().ok_or("--pid needs a value")?;
                opts.pids = Some(parse_pids(&value)?);
            }
            "--user" => {
                let value = args.next().ok_or("--user needs a value")?;
                opts.user = Some(value);
            }
            "--proc-root" => {
                let value = args.next().ok_or("--proc-root needs a value")?;
                opts.proc_root = PathBuf::from(value);
            }
            other => {
                if let Some(value) = other.strip_prefix("--sort=") {
                    opts.sort = parse_sort(value)?;
                } else if let Some(value) = other.strip_prefix("--limit=") {
                    opts.limit = Some(parse_number(value, "--limit")?);
                } else if let Some(value) = other.strip_prefix("--pid=") {
                    opts.pids = Some(parse_pids(value)?);
                } else if let Some(value) = other.strip_prefix("--user=") {
                    opts.user = Some(value.to_string());
                } else if let Some(value) = other.strip_prefix("--proc-root=") {
                    opts.proc_root = PathBuf::from(value);
                } else {
                    return Err(format!("unknown option '{other}'"));
                }
            }
        }
    }
    Ok(Some(opts))
}

fn parse_sort(value: &str) -> Result<SortKey, String> {
    match value {
        "pid" => Ok(SortKey::Pid),
        "ppid" => Ok(SortKey::Ppid),
        "cpu" => Ok(SortKey::Cpu),
        "rss" => Ok(SortKey::Rss),
        "vsz" => Ok(SortKey::Vsz),
        "name" => Ok(SortKey::Name),
        _ => Err(format!(
            "unknown sort key '{value}' (try pid, ppid, cpu, rss, vsz, name)"
        )),
    }
}

fn parse_number(value: &str, name: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("{name} expects a non-negative number, got '{value}'"))
}

fn parse_pids(value: &str) -> Result<BTreeSet<u32>, String> {
    let mut pids = BTreeSet::new();
    for item in value.split(',').filter(|item| !item.is_empty()) {
        let pid: u32 = item.parse().map_err(|_| format!("invalid pid '{item}'"))?;
        pids.insert(pid);
    }
    if pids.is_empty() {
        return Err("--pid needs at least one pid".to_string());
    }
    Ok(pids)
}

fn read_processes(
    opts: &Options,
    users: &HashMap<u32, String>,
    uptime: f64,
    details: bool,
) -> io::Result<Vec<Process>> {
    let mut processes = Vec::new();
    for entry in fs::read_dir(&opts.proc_root)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|text| text.parse::<u32>().ok()) else {
            continue;
        };
        if let Some(process) = read_process(&opts.proc_root, pid, users, uptime, details) {
            processes.push(process);
        }
    }
    Ok(processes)
}

fn read_process(
    proc_root: &Path,
    pid: u32,
    users: &HashMap<u32, String>,
    uptime: f64,
    long: bool,
) -> Option<Process> {
    let dir = proc_root.join(pid.to_string());
    let stat_text = fs::read_to_string(dir.join("stat")).ok()?;
    let stat = parse_stat(&stat_text)?;
    let status = fs::read_to_string(dir.join("status"))
        .ok()
        .map(|text| parse_status(&text))
        .unwrap_or_default();
    let smaps = if long {
        fs::read_to_string(dir.join("smaps_rollup"))
            .ok()
            .map(|text| parse_smaps(&text))
            .unwrap_or_default()
    } else {
        Smaps::default()
    };
    let command = fs::read(dir.join("cmdline"))
        .ok()
        .map(|bytes| parse_cmdline(&bytes))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("[{}]", stat.comm));

    let uid = status
        .uid
        .or_else(|| fs::metadata(&dir).ok().map(|meta| meta.uid()))
        .unwrap_or(0);
    let user = users.get(&uid).cloned().unwrap_or_else(|| uid.to_string());

    let cpu_usec = (stat.utime.saturating_add(stat.stime)).saturating_mul(1_000_000) / CLOCK_TICKS;
    let start_secs = stat.starttime as f64 / CLOCK_TICKS as f64;
    let elapsed = (uptime - start_secs).max(0.0);
    let cpu_percent = if elapsed > 0.0 {
        (cpu_usec as f64 / 1_000_000.0) / elapsed * 100.0
    } else {
        0.0
    };

    Some(Process {
        pid: stat.pid,
        ppid: stat.ppid,
        uid,
        user,
        state: stat.state,
        vsz: stat.vsize,
        rss: stat.rss_bytes,
        pss: smaps.pss,
        uss: smaps.uss,
        swap: status.vm_swap,
        cpu_usec,
        cpu_percent,
        threads: status.threads,
        command,
    })
}

fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let pid = text.get(..open)?.trim().parse().ok()?;
    let comm = text.get(open + 1..close)?.to_string();
    let fields: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let ppid = fields.get(1)?.parse().ok()?;
    let utime = fields.get(11)?.parse().ok()?;
    let stime = fields.get(12)?.parse().ok()?;
    let starttime = fields.get(19)?.parse().ok()?;
    let vsize = fields.get(20)?.parse().ok()?;
    let rss_pages = fields.get(21)?.parse::<i64>().ok()?.max(0) as u64;
    Some(Stat {
        pid,
        ppid,
        comm,
        state,
        utime,
        stime,
        starttime,
        vsize,
        rss_bytes: rss_pages.saturating_mul(PAGE_SIZE),
    })
}

fn parse_status(text: &str) -> Status {
    let mut status = Status::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key {
            "Uid" => {
                status.uid = value
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse().ok());
            }
            "Threads" => {
                status.threads = value.trim().parse().ok();
            }
            "VmSwap" => {
                status.vm_swap = parse_kib(value);
            }
            _ => {}
        }
    }
    status
}

fn parse_smaps(text: &str) -> Smaps {
    let mut pss = 0u64;
    let mut uss = 0u64;
    let mut have_pss = false;
    let mut have_uss = false;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = value
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        match key {
            "Pss" => {
                pss = pss.saturating_add(value);
                have_pss = true;
            }
            "Private_Clean" | "Private_Dirty" | "Private_Hugetlb" => {
                uss = uss.saturating_add(value);
                have_uss = true;
            }
            _ => {}
        }
    }
    Smaps {
        pss: have_pss.then(|| pss.saturating_mul(1024)),
        uss: have_uss.then(|| uss.saturating_mul(1024)),
    }
}

fn parse_kib(value: &str) -> Option<u64> {
    value
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.saturating_mul(1024))
}

fn parse_cmdline(bytes: &[u8]) -> String {
    bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn read_uptime(proc_root: &Path) -> Option<f64> {
    let text = fs::read_to_string(proc_root.join("uptime")).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

fn load_users() -> HashMap<u32, String> {
    let mut users = HashMap::new();
    let Ok(text) = fs::read_to_string("/etc/passwd") else {
        return users;
    };
    for line in text.lines() {
        let mut fields = line.split(':');
        let Some(name) = fields.next() else {
            continue;
        };
        let _password = fields.next();
        let Some(uid) = fields.next().and_then(|uid| uid.parse::<u32>().ok()) else {
            continue;
        };
        users.entry(uid).or_insert_with(|| name.to_string());
    }
    users
}

fn filter_processes(processes: &mut Vec<Process>, opts: &Options) {
    if let Some(pids) = &opts.pids {
        processes.retain(|process| pids.contains(&process.pid));
    }
    if let Some(user) = &opts.user {
        processes.retain(|process| {
            process.user.as_str() == user.as_str() || process.uid.to_string() == *user
        });
    }
}

fn sort_processes(processes: &mut [Process], opts: &Options) {
    match opts.sort {
        SortKey::Pid => processes.sort_by_key(|process| process.pid),
        SortKey::Ppid => processes.sort_by_key(|process| (process.ppid, process.pid)),
        SortKey::Cpu => {
            processes.sort_by(|a, b| b.cpu_usec.cmp(&a.cpu_usec).then_with(|| a.pid.cmp(&b.pid)))
        }
        SortKey::Rss => processes.sort_by(|a, b| b.rss.cmp(&a.rss).then_with(|| a.pid.cmp(&b.pid))),
        SortKey::Vsz => processes.sort_by(|a, b| b.vsz.cmp(&a.vsz).then_with(|| a.pid.cmp(&b.pid))),
        SortKey::Name => {
            processes.sort_by(|a, b| a.command.cmp(&b.command).then_with(|| a.pid.cmp(&b.pid)))
        }
    }
    if opts.reverse {
        processes.reverse();
    }
}

fn sort_for_tree(processes: &mut [Process]) {
    processes.sort_by_key(|process| process.pid);
}

fn print_table(processes: &[Process], opts: &Options) {
    let mut rows = Vec::new();
    for process in processes {
        rows.push(table_row(process, "", opts.long));
    }
    print_rows(rows, opts);
}

fn print_tree(processes: &[Process], opts: &Options) {
    let by_pid: BTreeMap<u32, &Process> = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect();
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for process in processes {
        children.entry(process.ppid).or_default().push(process.pid);
    }
    for list in children.values_mut() {
        list.sort_unstable();
    }
    let mut roots: Vec<u32> = processes
        .iter()
        .filter(|process| process.ppid == 0 || !by_pid.contains_key(&process.ppid))
        .map(|process| process.pid)
        .collect();
    roots.sort_unstable();

    let mut tree = TreeContext {
        by_pid: &by_pid,
        children: &children,
        visited: BTreeSet::new(),
        rows: Vec::new(),
        ascii: opts.ascii,
        long: opts.long,
    };
    for (index, pid) in roots.iter().enumerate() {
        let last = index + 1 == roots.len();
        tree.walk(*pid, "", last, true);
    }
    print_rows(tree.rows, opts);
}

struct TreeContext<'a> {
    by_pid: &'a BTreeMap<u32, &'a Process>,
    children: &'a BTreeMap<u32, Vec<u32>>,
    visited: BTreeSet<u32>,
    rows: Vec<Vec<String>>,
    ascii: bool,
    long: bool,
}

impl TreeContext<'_> {
    fn walk(&mut self, pid: u32, prefix: &str, last: bool, root: bool) {
        if !self.visited.insert(pid) {
            return;
        }
        let Some(process) = self.by_pid.get(&pid) else {
            return;
        };
        let connector = if root {
            String::new()
        } else if self.ascii {
            if last {
                "+- ".to_string()
            } else {
                "|- ".to_string()
            }
        } else if last {
            "└─ ".to_string()
        } else {
            "├─ ".to_string()
        };
        self.rows.push(table_row(
            process,
            &format!("{prefix}{connector}"),
            self.long,
        ));

        let child_prefix = if root {
            String::new()
        } else if self.ascii {
            format!("{prefix}{}", if last { "   " } else { "|  " })
        } else {
            format!("{prefix}{}", if last { "   " } else { "│  " })
        };
        let Some(list) = self.children.get(&pid) else {
            return;
        };
        for (index, child) in list.iter().enumerate() {
            let child_last = index + 1 == list.len();
            self.walk(*child, &child_prefix, child_last, false);
        }
    }
}

fn table_row(process: &Process, prefix: &str, long: bool) -> Vec<String> {
    let mut row = vec![
        process.pid.to_string(),
        process.ppid.to_string(),
        process.user.clone(),
        process.state.to_string(),
        format_bytes(process.vsz),
        format_bytes(process.rss),
        format!("{:.1}", process.cpu_percent),
        format_time(process.cpu_usec),
    ];
    if long {
        row.push(format_optional_bytes(process.pss));
        row.push(format_optional_bytes(process.uss));
        row.push(format_optional_bytes(process.swap));
    }
    if prefix.is_empty() {
        row.push(process.command.clone());
    } else {
        row.push(format!("{prefix}{}", process.command));
    }
    row
}

fn print_rows(rows: Vec<Vec<String>>, opts: &Options) {
    let mut headers: Vec<String> = ["PID", "PPID", "USER", "STAT", "VSZ", "RSS", "%CPU", "TIME+"]
        .iter()
        .map(|value| (*value).to_string())
        .collect();
    if opts.long {
        headers.extend(["PSS", "USS", "SWAP"].map(str::to_string));
    }
    headers.push("COMMAND".to_string());

    let mut width = headers.iter().map(|value| value.len()).collect::<Vec<_>>();
    for row in &rows {
        for (index, value) in row.iter().enumerate() {
            if let Some(slot) = width.get_mut(index) {
                *slot = (*slot).max(value.chars().count());
            }
        }
    }

    if !opts.no_header {
        println!("{}", format_row(&headers, &width));
    }
    for row in &rows {
        println!("{}", format_row(row, &width));
    }
}

fn print_json(processes: &[Process]) {
    let rows = processes
        .iter()
        .map(|process| {
            format!(
                "{{\"pid\":{},\"ppid\":{},\"user\":{},\"state\":{},\"vsz_bytes\":{},\"rss_bytes\":{},\"pss_bytes\":{},\"uss_bytes\":{},\"swap_bytes\":{},\"cpu_usec\":{},\"cpu_percent\":{:.2},\"threads\":{},\"command\":{}}}",
                process.pid,
                process.ppid,
                json_string(&process.user),
                json_string(&process.state.to_string()),
                process.vsz,
                process.rss,
                json_num(process.pss),
                json_num(process.uss),
                json_num(process.swap),
                process.cpu_usec,
                process.cpu_percent,
                json_num(process.threads),
                json_string(&process.command)
            )
        })
        .collect::<Vec<_>>();
    println!("[{}]", rows.join(","));
}

fn format_row(values: &[String], width: &[usize]) -> String {
    values
        .iter()
        .zip(width.iter())
        .map(|(value, width)| format!("{value:<width$}", width = *width))
        .collect::<Vec<_>>()
        .join("  ")
}

fn format_optional_bytes(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "-".to_string(), format_bytes)
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn format_time(usec: u64) -> String {
    let seconds = usec / 1_000_000;
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes > 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{seconds}.{:02}", (usec % 1_000_000) / 10_000)
    }
}

fn json_num(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_string())
}

fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch < ' ' => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn non_utf8_arguments_are_a_usage_error() {
        let bad = OsString::from_vec(vec![b'x', 0xff]);
        assert_eq!(collect_args(std::iter::once(bad.clone())), Err(bad));
    }

    #[test]
    fn stat_is_parsed_without_being_confused_by_spaces_in_comm() {
        let stat = "42 (hello world) S 1 42 42 0 -1 4194560 100 0 0 0 12 34 0 0 20 0 1 0 12345 1000 25 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let parsed = parse_stat(stat).expect("parse stat");
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.ppid, 1);
        assert_eq!(parsed.comm, "hello world");
        assert_eq!(parsed.state, 'S');
        assert_eq!(parsed.utime, 12);
        assert_eq!(parsed.stime, 34);
        assert_eq!(parsed.starttime, 12345);
        assert_eq!(parsed.vsize, 1000);
        assert_eq!(parsed.rss_bytes, 25 * PAGE_SIZE);
    }

    #[test]
    fn status_and_smaps_feed_the_long_columns() {
        let status = parse_status("Uid:\t1000\t1000\t1000\t1000\nThreads:\t3\nVmSwap:\t64 kB\n");
        assert_eq!(status.uid, Some(1000));
        assert_eq!(status.threads, Some(3));
        assert_eq!(status.vm_swap, Some(64 * 1024));

        let smaps = parse_smaps(
            "Pss:\t10 kB\nPrivate_Clean:\t4 kB\nPrivate_Dirty:\t6 kB\nPrivate_Hugetlb:\t1 kB\n",
        );
        assert_eq!(smaps.pss, Some(10 * 1024));
        assert_eq!(smaps.uss, Some(11 * 1024));
    }

    #[test]
    fn cmdline_is_nul_separated_and_empty_falls_back_at_the_caller() {
        assert_eq!(
            parse_cmdline(b"/bin/sh\0-c\0echo hi\0"),
            "/bin/sh -c echo hi"
        );
        assert!(parse_cmdline(b"\0").is_empty());
    }

    #[test]
    fn format_helpers_are_stable() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(1024), "1.0K");
        assert_eq!(format_bytes(1024 * 1024), "1.0M");
        assert_eq!(format_time(1_234_000), "1.23");
        assert_eq!(format_time(62_000_000), "1:02");
    }
}
