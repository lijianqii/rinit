//! `rlogin` - authenticate a user and start their login shell.

mod crypt;

use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};

use rinit_tools::{EXIT_OK, program_name, usage_error};

#[derive(Clone)]
struct User {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
    shell: String,
    password: String,
}

fn parse_passwd(text: &str, name: &str) -> Option<User> {
    for line in text.lines() {
        let mut fields = line.split(':');
        let Some(field_name) = fields.next() else {
            continue;
        };
        if field_name != name {
            continue;
        }
        let Some(password) = fields.next() else {
            continue;
        };
        let Some(uid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(gid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let _gecos = fields.next();
        let home = fields.next().unwrap_or("/");
        let shell = fields.next().unwrap_or("/bin/sh");
        return Some(User {
            name: field_name.to_string(),
            uid,
            gid,
            home: if home.is_empty() { "/" } else { home }.to_string(),
            shell: if shell.is_empty() { "/bin/sh" } else { shell }.to_string(),
            password: password.to_string(),
        });
    }
    None
}

fn parse_shadow_hash(text: &str, name: &str) -> Option<String> {
    shadow_fields(text, name)?
        .get(1)
        .map(|hash| (*hash).to_string())
}

fn shadow_fields<'a>(text: &'a str, name: &str) -> Option<Vec<&'a str>> {
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.first().copied() == Some(name) {
            return Some(fields);
        }
    }
    None
}

/// Days since the Unix epoch, the unit `/etc/shadow` uses for its date fields.
fn today_in_days() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() / 86_400)
        .unwrap_or(0)
}

/// Whether the shadow record forbids this login regardless of the password.
///
/// `rlogin` cannot run `passwd` to force a change, so an expired password or an
/// expired account is refused instead of being allowed through. A malformed
/// date is also refused: the record exists, so it cannot be read as "never
/// expires".
fn shadow_login_denied(text: &str, name: &str, today: u64) -> bool {
    let Some(fields) = shadow_fields(text, name) else {
        return false;
    };
    // [2] last change, [4] maximum age, [7] account expiration. All are days
    // since the epoch; empty means "not set". A value that exists but does not
    // parse is refused: the record is there, so it cannot be read as "never
    // expires".
    fn number(field: Option<&&str>) -> Result<Option<u64>, ()> {
        match field {
            None | Some(&"") => Ok(None),
            Some(value) => value.parse::<u64>().map(Some).map_err(|_| ()),
        }
    }
    let last_change = match number(fields.get(2)) {
        Ok(value) => value,
        Err(()) => return true,
    };
    let max_age = match number(fields.get(4)) {
        Ok(value) => value,
        Err(()) => return true,
    };
    // `last_change == 0` means "change it at the next login", and
    // `max_age == 0` means the password is already expired. rlogin cannot run
    // passwd, so both are denials. A future last-change date is malformed for
    // this purpose and is refused rather than treated as an unlimited age.
    if last_change == Some(0) || max_age == Some(0) {
        return true;
    }
    if last_change.is_some_and(|last_change| last_change > today) {
        return true;
    }
    if let (Some(last_change), Some(max_age)) = (last_change, max_age)
        && today >= last_change.saturating_add(max_age)
    {
        return true;
    }

    match fields.get(7) {
        None | Some(&"") => false,
        Some(value) => match value.parse::<u64>() {
            Ok(expire) => today >= expire,
            Err(_) => true,
        },
    }
}

fn password_hash_from(field: &str, shadow: Option<&str>, name: &str) -> Option<String> {
    if field != "x" && !field.is_empty() {
        return Some(field.to_string());
    }
    shadow.and_then(|text| parse_shadow_hash(text, name))
}

fn password_hash(user: &User) -> Option<String> {
    let shadow = fs::read_to_string("/etc/shadow").ok();
    if let Some(text) = shadow.as_deref()
        && shadow_login_denied(text, &user.name, today_in_days())
    {
        return None;
    }
    password_hash_from(&user.password, shadow.as_deref(), &user.name)
}

fn verify_hash(password: &str, hash: &str) -> bool {
    if hash.is_empty() {
        return true;
    }
    if hash.starts_with('!') || hash.starts_with('*') {
        return false;
    }
    if !(hash.starts_with("$6$") || hash.starts_with("$5$") || hash.starts_with("$1$")) {
        return false;
    }
    crypt::verify(password, hash)
}

fn ignore_job_control_signals() {
    // SAFETY: fixed process-wide signal dispositions.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTOU] {
            libc::signal(signal, libc::SIG_IGN);
        }
    }
}

fn default_job_control_signals() {
    // SAFETY: restore dispositions inherited across exec.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTOU] {
            libc::signal(signal, libc::SIG_DFL);
        }
    }
}

fn read_raw_line() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn read_line(prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    read_raw_line()
}

fn read_password(prompt: &str) -> io::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    // SAFETY: `isatty`, `tcgetattr` and `tcsetattr` operate on fd 0. The saved
    // attributes are restored before returning, including on read errors.
    let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
    let saved =
        if unsafe { libc::isatty(0) } == 1 && unsafe { libc::tcgetattr(0, &mut original) } == 0 {
            let mut hidden = original;
            hidden.c_lflag &= !libc::ECHO;
            let _ = unsafe { libc::tcsetattr(0, libc::TCSANOW, &hidden) };
            Some(original)
        } else {
            None
        };
    let result = read_raw_line();
    if let Some(original) = saved {
        let _ = unsafe { libc::tcsetattr(0, libc::TCSANOW, &original) };
    }
    println!();
    result
}

fn authenticate(user: &User, force: bool) -> io::Result<bool> {
    if force {
        return Ok(unsafe { libc::geteuid() } == 0);
    }
    let hash = password_hash(user);
    let Some(hash) = hash else {
        return Ok(false);
    };
    if hash.is_empty() {
        return Ok(true);
    }
    if hash.starts_with('!') || hash.starts_with('*') {
        return Ok(false);
    }
    let password = read_password("Password: ")?;
    Ok(verify_hash(&password, &hash))
}

fn short_tty(name: &str) -> String {
    name.strip_prefix("/dev/").unwrap_or(name).to_string()
}

fn securetty_allows(short: &str, text: Option<&str>) -> bool {
    let Some(text) = text else {
        return true;
    };
    let mut listed = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        listed = true;
        if line == short || line == format!("/dev/{short}") {
            return true;
        }
    }
    !listed
}

fn tty_name() -> Option<String> {
    let mut buffer = [0 as libc::c_char; 128];
    // SAFETY: `buffer` is writable storage for the NUL-terminated result.
    if unsafe { libc::ttyname_r(0, buffer.as_mut_ptr(), buffer.len()) } != 0 {
        return None;
    }
    // SAFETY: ttyname_r succeeded, so the buffer is NUL-terminated.
    Some(
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn secure_tty() -> bool {
    let Some(name) = tty_name() else {
        return true;
    };
    let text = fs::read_to_string("/etc/securetty").ok();
    securetty_allows(&short_tty(&name), text.as_deref())
}

fn nologin_text(user: &User, text: Option<&str>) -> bool {
    user.uid != 0 && text.is_some()
}

fn nologin(user: &User) -> bool {
    let text = fs::read_to_string("/etc/nologin").ok();
    if !nologin_text(user, text.as_deref()) {
        return false;
    }
    if let Some(text) = text {
        if text.is_empty() {
            println!("System closed for routine maintenance");
        } else {
            print!("{text}");
        }
        let _ = io::stdout().flush();
    }
    true
}

fn change_identity(user: &User) -> Result<(), String> {
    let name =
        CString::new(user.name.as_str()).map_err(|_| "user name contains NUL".to_string())?;
    // The tty must be handed to the user while we still have CAP_CHOWN/FOWNER.
    // SAFETY: fd 0 is the console handed over by getty.
    if unsafe { libc::fchown(0, user.uid as libc::uid_t, user.gid as libc::gid_t) } != 0 {
        return Err(format!("fchown tty: {}", io::Error::last_os_error()));
    }
    if unsafe { libc::fchmod(0, 0o600) } != 0 {
        return Err(format!("fchmod tty: {}", io::Error::last_os_error()));
    }
    // SAFETY: `name` is a valid C string; the IDs come from /etc/passwd.
    let groups = unsafe { libc::initgroups(name.as_ptr(), user.gid as libc::gid_t) };
    if groups != 0 && user.uid != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    // SAFETY: `setgid`/`setuid` are the standard privilege drop sequence.
    if unsafe { libc::setgid(user.gid as libc::gid_t) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    if unsafe { libc::setuid(user.uid as libc::uid_t) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    // SAFETY: these queries have no preconditions.
    let (uid, euid, gid, egid) = unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    };
    if uid != user.uid || euid != user.uid || gid != user.gid || egid != user.gid {
        return Err("identity did not drop to the requested user".to_string());
    }
    // SAFETY: `umask` is process-local and has no failure mode.
    unsafe {
        libc::umask(0o022);
    }
    Ok(())
}

fn shell_environment(
    user: &User,
    preserve: bool,
    host: Option<&str>,
    term: Option<&str>,
) -> Vec<(String, String)> {
    let home = if Path::new(&user.home).is_dir() {
        user.home.clone()
    } else {
        "/".to_string()
    };
    let shell = if user.shell.is_empty() {
        "/bin/sh".to_string()
    } else {
        user.shell.clone()
    };
    let mut values = Vec::new();
    if !preserve && let Some(term) = term {
        values.push(("TERM".to_string(), term.to_string()));
    }
    values.push(("HOME".to_string(), home));
    values.push(("SHELL".to_string(), shell));
    values.push(("USER".to_string(), user.name.clone()));
    values.push(("LOGNAME".to_string(), user.name.clone()));
    values.push((
        "PATH".to_string(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
    ));
    if let Some(host) = host {
        values.push(("REMOTE".to_string(), host.to_string()));
    }
    values
}

fn start_shell(user: &User, preserve: bool, host: Option<&str>) -> Result<(), String> {
    let shell = if user.shell.is_empty() {
        "/bin/sh".to_string()
    } else {
        user.shell.clone()
    };
    let home = if Path::new(&user.home).is_dir() {
        user.home.clone()
    } else {
        "/".to_string()
    };
    let term = std::env::var("TERM").ok();
    let mut command = Command::new(&shell);
    command.current_dir(&home);
    command.arg0(format!(
        "-{}",
        Path::new(&shell)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sh")
    ));
    if !preserve {
        command.env_clear();
    }
    for (name, value) in shell_environment(user, preserve, host, term.as_deref()) {
        command.env(name, value);
    }
    let error = command.exec();
    Err(format!("cannot execute {shell}: {error}"))
}

struct Arguments {
    username: String,
    host: Option<String>,
    preserve: bool,
    force_user: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Arguments, String> {
    let mut parsed = Arguments {
        username: String::new(),
        host: None,
        preserve: false,
        force_user: None,
    };
    let mut index = 0;
    let mut options = true;
    while let Some(arg) = args.get(index) {
        if options {
            match arg.as_str() {
                "--" => {
                    options = false;
                    index += 1;
                    continue;
                }
                "-p" => {
                    parsed.preserve = true;
                    index += 1;
                    continue;
                }
                "-f" => {
                    index += 1;
                    let Some(value) = args.get(index) else {
                        return Err("-f needs a user".to_string());
                    };
                    parsed.username = value.clone();
                    parsed.force_user = Some(value.clone());
                    index += 1;
                    continue;
                }
                "-h" => {
                    index += 1;
                    let Some(value) = args.get(index) else {
                        return Err("-h needs a host".to_string());
                    };
                    parsed.host = Some(value.clone());
                    index += 1;
                    continue;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unknown option '{value}'"));
                }
                value => {
                    if parsed.username.is_empty() {
                        parsed.username = value.to_string();
                    } else {
                        return Err("too many arguments".to_string());
                    }
                    options = false;
                    index += 1;
                    continue;
                }
            }
        }
        if parsed.username.is_empty() {
            parsed.username = arg.clone();
        } else {
            return Err("too many arguments".to_string());
        }
        index += 1;
    }
    Ok(parsed)
}

fn run(args: &[String], program: &str) -> ExitCode {
    if args.iter().any(|arg| arg == "--help") {
        println!("Usage: rlogin [-p] [-h HOST] [[-f] USER]");
        return ExitCode::from(EXIT_OK);
    }
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return ExitCode::from(usage_error(program, &error)),
    };
    ignore_job_control_signals();
    let passwd = match fs::read_to_string("/etc/passwd") {
        Ok(text) => text,
        Err(error) => {
            eprintln!("{program}: /etc/passwd: {error}");
            return ExitCode::from(1);
        }
    };
    let mut username = parsed.username;
    let mut failures = 0;
    let user = loop {
        if username.is_empty() {
            match read_line("login: ") {
                Ok(value) => username = value,
                Err(error) => {
                    eprintln!("{program}: {error}");
                    return ExitCode::from(1);
                }
            }
            if username.is_empty() {
                failures += 1;
                if failures >= 3 {
                    return ExitCode::from(1);
                }
                continue;
            }
        }
        let candidate = parse_passwd(&passwd, &username);
        let force = parsed.force_user.as_deref() == Some(username.as_str());
        let ok = match candidate.as_ref() {
            Some(candidate) if candidate.uid == 0 && !secure_tty() => false,
            Some(candidate) => match authenticate(candidate, force) {
                Ok(ok) => ok,
                Err(error) => {
                    eprintln!("{program}: {error}");
                    return ExitCode::from(1);
                }
            },
            None => {
                let _ = read_password("Password: ");
                false
            }
        };
        if ok && let Some(candidate) = candidate {
            break candidate;
        }
        eprintln!("Login incorrect");
        std::thread::sleep(std::time::Duration::from_secs(2));
        failures += 1;
        if failures >= 3 {
            return ExitCode::from(1);
        }
        username.clear();
    };
    if nologin(&user) {
        return ExitCode::from(1);
    }
    if let Some(tty) = tty_name()
        && let Err(error) = rinit_tools::utmp::user_record(
            unsafe { libc::getpid() },
            &tty,
            &user.name,
            parsed.host.as_deref(),
        )
    {
        eprintln!("{program}: utmp: {error}");
    }
    if let Err(error) = change_identity(&user) {
        eprintln!("{program}: {error}");
        return ExitCode::from(1);
    }
    default_job_control_signals();
    if let Err(error) = start_shell(&user, parsed.preserve, parsed.host.as_deref()) {
        eprintln!("{program}: {error}");
        return ExitCode::from(1);
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
        "rlogin",
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
    fn passwd_and_shadow_parsers_find_entries() {
        let passwd = "root:x:0:0:root:/root:/bin/sh\nuser:x:1000:1000::/home/user:/bin/bash\n";
        let root = parse_passwd(passwd, "root").expect("root");
        assert_eq!(root.uid, 0);
        assert_eq!(root.gid, 0);
        assert_eq!(root.home, "/root");
        assert_eq!(root.shell, "/bin/sh");
        assert_eq!(root.password, "x");
        let shadow = "root:$6$salt$hash:0:0:99999:7:::\nuser:!:1000:0:99999:7:::\n";
        assert_eq!(
            parse_shadow_hash(shadow, "root").as_deref(),
            Some("$6$salt$hash")
        );
        assert_eq!(parse_shadow_hash(shadow, "missing"), None);
    }

    /// Expiry is a login decision, not just a hash lookup: a valid password on
    /// an expired account or after its maximum age must still be refused, and a
    /// malformed date must fail closed.
    #[test]
    fn shadow_expiry_and_aging_deny_the_login() {
        let today = 20_000;
        // last change, max age, warning, inactive, account expiry.
        let live = "root:$6$h:19000:0:99999:7:::";
        assert!(!shadow_login_denied(live, "root", today));

        let account_expired = "root:$6$h:19000:0:99999:7::19999:";
        assert!(shadow_login_denied(account_expired, "root", today));

        let password_expired = "root:$6$h:19000:0:30:7:::";
        assert!(!shadow_login_denied(password_expired, "root", 19_029));
        assert!(shadow_login_denied(password_expired, "root", 19_030));

        let no_aging = "root:$6$h:19000:0::::";
        assert!(!shadow_login_denied(no_aging, "root", today));

        let must_change = "root:$6$h:0:0:99999:7:::";
        assert!(shadow_login_denied(must_change, "root", today));
        let max_age_zero = "root:$6$h:19000:0:0:7:::";
        assert!(shadow_login_denied(max_age_zero, "root", today));
        let future_change = "root:$6$h:30000:0:99999:7:::";
        assert!(shadow_login_denied(future_change, "root", today));

        let malformed = "root:$6$h:19000:0:99999:7::soon:";
        assert!(
            shadow_login_denied(malformed, "root", today),
            "an unreadable expiry date must not be treated as no expiry"
        );
        assert!(!shadow_login_denied(live, "missing", today));
    }

    fn user(uid: u32) -> User {
        User {
            name: "root".to_string(),
            uid,
            gid: uid,
            home: "/root".to_string(),
            shell: "/bin/sh".to_string(),
            password: "x".to_string(),
        }
    }

    #[test]
    fn malformed_passwd_shadow_and_option_records_are_rejected() {
        assert!(parse_passwd("short:x:0\n", "short").is_none());
        assert!(parse_passwd("missing:x:not-a-uid:0::/:/bin/sh\n", "missing").is_none());
        assert!(parse_shadow_hash("malformed\n", "malformed").is_none());
        assert!(shadow_login_denied(
            "root:$6$h:soon:0:99999:7:::",
            "root",
            1
        ));
        assert!(shadow_login_denied("root:$6$h:0:0:soon:7:::", "root", 1));
        assert_eq!(password_hash_from("", None, "root"), None);
        assert_eq!(
            password_hash_from("x", Some("root:$6$h:0:0:99999:7:::\n"), "root").as_deref(),
            Some("$6$h")
        );
        for args in [
            ["-l"].as_slice(),
            ["-h"].as_slice(),
            ["-f"].as_slice(),
            ["-p", "-h"].as_slice(),
        ] {
            assert!(
                parse_args(
                    &args
                        .iter()
                        .map(|arg| (*arg).to_string())
                        .collect::<Vec<_>>()
                )
                .is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn password_hash_uses_passwd_or_shadow() {
        assert_eq!(
            password_hash_from("$6$direct$hash", None, "root").as_deref(),
            Some("$6$direct$hash")
        );
        assert_eq!(
            password_hash_from("x", Some("root:$6$shadow$hash:0:0:99999:7:::\n"), "root")
                .as_deref(),
            Some("$6$shadow$hash")
        );
        assert_eq!(password_hash_from("x", None, "root"), None);
        assert_eq!(
            password_hash_from("x", Some("root:$6$shadow$hash:0:0:99999:7:::\n"), "missing"),
            None
        );
    }

    #[test]
    fn password_verification_handles_locked_and_empty_hashes() {
        let hash = "$6$rinit$h.sfVK5a75HqcCQ8mjFVlpyRSK5rin/8GpC7okbfq82BH16wN0lJDJUvCd.0bpt2/q/MtfkobUfMxtG72uIuj.";
        assert!(verify_hash("toor", hash));
        assert!(!verify_hash("wrong", hash));
        assert!(verify_hash("anything", ""));
        assert!(!verify_hash("anything", "!"));
        assert!(!verify_hash("anything", "*"));
        assert!(!verify_hash("anything", "$2b$salt$hash"));
        assert!(verify_hash(
            "toor",
            "$5$rinit$3J/IImUzg9LF.7ZabNp3wB5/C1QgamiQI92FXaJf7s/"
        ));
        assert!(verify_hash("toor", "$1$rinit$TgEj7YentAlBjR1IVAQLG1"));
    }

    #[test]
    fn securetty_is_honoured_only_when_populated() {
        assert!(securetty_allows("ttyAMA0", None));
        assert!(securetty_allows("ttyAMA0", Some("")));
        assert!(securetty_allows("ttyAMA0", Some("# comment\n\n")));
        assert!(securetty_allows(
            "ttyAMA0",
            Some("ttyS0\nttyAMA0 # console\n")
        ));
        assert!(securetty_allows("ttyAMA0", Some("/dev/ttyAMA0\n")));
        assert!(!securetty_allows("ttyS0", Some("ttyAMA0\n")));
        assert_eq!(short_tty("/dev/ttyAMA0"), "ttyAMA0");
        assert_eq!(short_tty("ttyAMA0"), "ttyAMA0");
    }

    #[test]
    fn nologin_only_denies_non_root() {
        assert!(!nologin_text(&user(0), Some("maintenance")));
        assert!(nologin_text(&user(1000), Some("maintenance")));
        assert!(!nologin_text(&user(1000), None));
    }

    #[test]
    fn passwd_defaults_fill_empty_home_and_shell() {
        let parsed = parse_passwd("user:x:1000:1000:::/bin/sh\n", "user").expect("user");
        assert_eq!(parsed.home, "/");
        assert!(parse_passwd("short:line\n", "short").is_none());
    }

    #[test]
    fn shell_environment_covers_preserve_and_remote() {
        let mut user = user(1000);
        user.home = "/definitely/not/a/directory".to_string();
        user.shell = String::new();
        let values = shell_environment(&user, false, Some("host"), Some("xterm"));
        let map: std::collections::BTreeMap<_, _> = values.into_iter().collect();
        assert_eq!(map.get("HOME").map(String::as_str), Some("/"));
        assert_eq!(map.get("SHELL").map(String::as_str), Some("/bin/sh"));
        assert_eq!(map.get("USER").map(String::as_str), Some("root"));
        assert_eq!(map.get("LOGNAME").map(String::as_str), Some("root"));
        assert_eq!(map.get("TERM").map(String::as_str), Some("xterm"));
        assert_eq!(map.get("REMOTE").map(String::as_str), Some("host"));
        assert!(map.contains_key("PATH"));
        let values = shell_environment(&user, true, None, Some("xterm"));
        assert!(!values.iter().any(|(name, _)| name == "TERM"));
    }

    #[test]
    fn argument_parser_handles_common_forms() {
        let parsed = parse_args(&["root".to_string()]).expect("user");
        assert_eq!(parsed.username, "root");
        assert_eq!(parsed.host, None);
        assert!(!parsed.preserve);
        assert_eq!(parsed.force_user, None);

        let parsed =
            parse_args(&["-p".to_string(), "-h".to_string(), "host".to_string()]).expect("opts");
        assert_eq!(parsed.username, "");
        assert_eq!(parsed.host.as_deref(), Some("host"));
        assert!(parsed.preserve);
        assert_eq!(parsed.force_user, None);

        let parsed = parse_args(&["-f".to_string(), "root".to_string()]).expect("force");
        assert_eq!(parsed.username, "root");
        assert_eq!(parsed.force_user.as_deref(), Some("root"));
        let parsed = parse_args(&["--".to_string(), "root".to_string()]).expect("dashdash");
        assert_eq!(parsed.username, "root");
        assert!(parse_args(&["-h".to_string()]).is_err());
        assert!(parse_args(&["a".to_string(), "b".to_string()]).is_err());
    }

    #[test]
    fn job_control_signals_are_ignored_and_restored() {
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        ignore_job_control_signals();
        // SAFETY: a null handler queries the current disposition.
        unsafe { libc::sigaction(libc::SIGINT, std::ptr::null(), &mut action) };
        assert_eq!(action.sa_sigaction, libc::SIG_IGN);
        default_job_control_signals();
        // SAFETY: a null handler queries the current disposition.
        unsafe { libc::sigaction(libc::SIGINT, std::ptr::null(), &mut action) };
        assert_eq!(action.sa_sigaction, libc::SIG_DFL);
    }

    #[test]
    fn run_handles_help_and_bad_options() {
        assert_eq!(
            run(&["--help".to_string()], "rlogin"),
            ExitCode::from(EXIT_OK)
        );
        assert_eq!(
            run(&["--bad".to_string()], "rlogin"),
            ExitCode::from(rinit_tools::EXIT_USAGE)
        );
    }
}
