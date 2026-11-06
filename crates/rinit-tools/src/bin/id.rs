//! `id` - print user and group identities.

use std::process::ExitCode;

use rinit_tools::users;
use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone, Copy, Default)]
struct Options {
    user: bool,
    group: bool,
    groups: bool,
    names: bool,
    real: bool,
}

fn id_value(options: Options) -> (u32, u32) {
    let uid = if options.real {
        // SAFETY: `getuid` has no preconditions.
        unsafe { libc::getuid() }
    } else {
        // SAFETY: `geteuid` has no preconditions.
        unsafe { libc::geteuid() }
    };
    let gid = if options.real {
        // SAFETY: `getgid` has no preconditions.
        unsafe { libc::getgid() }
    } else {
        // SAFETY: `getegid` has no preconditions.
        unsafe { libc::getegid() }
    };
    (uid, gid)
}

fn id_groups(gid: u32) -> Vec<u32> {
    // SAFETY: a zero-sized query with a null buffer is the documented way to
    // ask `getgroups` for the number of supplementary groups.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count < 0 {
        return vec![gid];
    }
    let mut values = vec![0; count as usize];
    // SAFETY: `values` has room for `count` entries.
    let got = unsafe { libc::getgroups(count, values.as_mut_ptr()) };
    if got < 0 {
        return vec![gid];
    }
    values.truncate(got as usize);
    if !values.contains(&gid) {
        values.insert(0, gid);
    }
    values
}

fn named(id: u32, name: Option<String>) -> String {
    match name {
        Some(name) => format!("{id}({name})"),
        None => id.to_string(),
    }
}

fn label(id: u32, names: bool, lookup: fn(u32) -> Option<String>) -> String {
    if names {
        // `-n` asks for the name alone; the default full output is what uses
        // the `id(name)` spelling.
        lookup(id).unwrap_or_else(|| id.to_string())
    } else {
        id.to_string()
    }
}

fn run(args: &[String], program: &str) -> ExitCode {
    let mut options = Options::default();
    for arg in args {
        match arg.as_str() {
            "--help" => {
                println!("Usage: id [-ugGnr]");
                return ExitCode::from(EXIT_OK);
            }
            "-u" => options.user = true,
            "-g" => options.group = true,
            "-G" => options.groups = true,
            "-n" => options.names = true,
            "-r" => options.real = true,
            value if value.starts_with('-') && value != "-" => {
                return ExitCode::from(usage_error(program, &format!("unknown option '{value}'")));
            }
            _ => return ExitCode::from(usage_error(program, "unexpected operand")),
        }
    }
    let (uid, gid) = id_value(options);
    if options.user || options.group || options.groups {
        if options.user {
            println!("{}", label(uid, options.names, users::user_name));
        }
        if options.group {
            println!("{}", label(gid, options.names, users::group_name));
        }
        if options.groups {
            let values = id_groups(gid);
            let labels: Vec<String> = values
                .iter()
                .map(|value| label(*value, options.names, users::group_name))
                .collect();
            println!("{}", labels.join(" "));
        }
        return ExitCode::from(EXIT_OK);
    }
    let groups = id_groups(gid);
    let groups = groups
        .iter()
        .map(|value| label(*value, true, users::group_name))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "uid={} gid={} groups={groups}",
        named(uid, users::user_name(uid)),
        named(gid, users::group_name(gid))
    );
    ExitCode::from(EXIT_OK)
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(
        std::env::args_os()
            .next()
            .as_deref()
            .and_then(|arg| arg.to_str()),
        "id",
    );
    let Some(args) = rinit_tools::utf8_args(&program) else {
        return ExitCode::from(rinit_tools::EXIT_USAGE);
    };
    let args: Vec<String> = args.into_iter().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_numbers_render() {
        assert_eq!(named(0, Some("root".to_string())), "0(root)");
        assert_eq!(named(1234, None), "1234");
        assert_eq!(label(0, false, users::user_name), "0");
    }

    #[test]
    fn run_handles_id_selectors() {
        for args in [
            Vec::<String>::new(),
            vec!["-u".to_string()],
            vec!["-g".to_string()],
            vec!["-G".to_string()],
            vec!["-u".to_string(), "-n".to_string()],
        ] {
            assert_eq!(run(&args, "id"), ExitCode::from(EXIT_OK));
        }
        assert_eq!(run(&["--help".to_string()], "id"), ExitCode::from(EXIT_OK));
        assert_eq!(
            run(&["--bad".to_string()], "id"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
