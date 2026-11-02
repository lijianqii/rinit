//! `free` - report memory usage from `/proc/meminfo`.

use std::collections::BTreeMap;
use std::io;
use std::process::ExitCode;

use rinit_tools::{EXIT_OK, program_name, read_trimmed, usage_error};

fn parse_meminfo(text: &str) -> BTreeMap<String, u64> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = rest.split_whitespace().next() else {
            continue;
        };
        if let Ok(value) = value.parse::<u64>() {
            values.insert(key.trim().to_string(), value);
        }
    }
    values
}

fn meminfo() -> io::Result<BTreeMap<String, u64>> {
    let text = read_trimmed(std::path::Path::new("/proc/meminfo"))?;
    Ok(parse_meminfo(&text))
}

fn field(values: &BTreeMap<String, u64>, key: &str) -> u64 {
    values.get(key).copied().unwrap_or(0)
}

fn scaled(kib: u64, unit: &str) -> String {
    match unit {
        "b" => format!("{}", kib.saturating_mul(1024)),
        "m" => format!("{}", kib / 1024),
        "g" => format!("{}", kib / (1024 * 1024)),
        "h" => human(kib),
        _ => format!("{kib}"),
    }
}

fn human(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.1}Gi", kib as f64 / (1024.0 * 1024.0))
    } else if kib >= 1024 {
        format!("{:.1}Mi", kib as f64 / 1024.0)
    } else {
        format!("{kib}Ki")
    }
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "free");
    let mut unit = "k".to_string();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--help" => {
                println!("Usage: free [-b|-k|-m|-g|-h]");
                return ExitCode::from(EXIT_OK);
            }
            "-b" | "-k" | "-m" | "-g" | "-h" => {
                unit = arg.trim_start_matches('-').to_string();
            }
            other => {
                return ExitCode::from(usage_error(&program, &format!("unknown option '{other}'")));
            }
        }
    }

    let values = match meminfo() {
        Ok(values) => values,
        Err(e) => {
            eprintln!("{program}: cannot read /proc/meminfo: {e}");
            return ExitCode::from(1);
        }
    };
    let total = field(&values, "MemTotal");
    let free = field(&values, "MemFree");
    let buffers = field(&values, "Buffers");
    let cached = field(&values, "Cached");
    let shared = field(&values, "Shmem");
    let available = field(&values, "MemAvailable").max(free);
    let used = total.saturating_sub(free + buffers + cached);
    let buff_cache = buffers + cached;

    println!("              total        used        free      shared  buff/cache   available");
    println!(
        "Mem:   {:>11} {:>11} {:>11} {:>11} {:>11} {:>11}",
        scaled(total, &unit),
        scaled(used, &unit),
        scaled(free, &unit),
        scaled(shared, &unit),
        scaled(buff_cache, &unit),
        scaled(available, &unit)
    );
    let swap_total = field(&values, "SwapTotal");
    let swap_free = field(&values, "SwapFree");
    if swap_total > 0 {
        println!(
            "Swap:  {:>11} {:>11} {:>11}",
            scaled(swap_total, &unit),
            scaled(swap_total.saturating_sub(swap_free), &unit),
            scaled(swap_free, &unit)
        );
    }
    ExitCode::from(EXIT_OK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_scale_and_humanize() {
        assert_eq!(scaled(1024, "k"), "1024");
        assert_eq!(scaled(1024, "m"), "1");
        assert_eq!(scaled(1024, "g"), "0");
        assert_eq!(scaled(1024, "b"), "1048576");
        assert_eq!(human(1024), "1.0Mi");
        assert_eq!(human(1024 * 1024), "1.0Gi");
    }

    #[test]
    fn meminfo_parser_reads_numeric_fields_only() {
        let values = parse_meminfo(
            "MemTotal:       2048 kB\nMemFree:  512 kB\nBad: nope\nSwapTotal: 1024 kB\n",
        );
        assert_eq!(values.get("MemTotal"), Some(&2048));
        assert_eq!(values.get("MemFree"), Some(&512));
        assert_eq!(values.get("SwapTotal"), Some(&1024));
        assert!(!values.contains_key("Bad"));
    }
}
