//! `rgetty` - open a terminal and hand it to `rlogin`.

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

use rinit_tools::{EXIT_OK, program_name, usage_error};

struct Options {
    local: bool,
    login: String,
    host: Option<String>,
    no_prompt: bool,
    tty: String,
    baud: u32,
    term: Option<String>,
}

fn parse_baud(value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("invalid baud rate '{value}'"))
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        local: false,
        login: "/bin/login".to_string(),
        host: None,
        no_prompt: false,
        tty: String::new(),
        baud: 115200,
        term: None,
    };
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "-L" | "--local" => options.local = true,
            "-n" | "--no-prompt" => options.no_prompt = true,
            "-l" | "--login" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("-l needs a login program".to_string());
                };
                options.login = value.clone();
            }
            "-h" | "--host" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("-h needs a host".to_string());
                };
                options.host = Some(value.clone());
            }
            value if value.starts_with('-') && value != "-" => {
                return Err(format!("unknown option '{value}'"));
            }
            value => positional.push(value.to_string()),
        }
        index += 1;
    }
    let mut positional_iter = positional.into_iter();
    let Some(tty) = positional_iter.next() else {
        return Err("a tty device is required".to_string());
    };
    options.tty = tty;
    if let Some(baud) = positional_iter.next() {
        options.baud = parse_baud(&baud)?;
    }
    if let Some(term) = positional_iter.next() {
        options.term = Some(term);
    }
    if positional_iter.next().is_some() {
        return Err("too many arguments".to_string());
    }
    Ok(options)
}

fn speed(baud: u32) -> Option<libc::speed_t> {
    match baud {
        1200 => Some(libc::B1200),
        2400 => Some(libc::B2400),
        4800 => Some(libc::B4800),
        9600 => Some(libc::B9600),
        19200 => Some(libc::B19200),
        38400 => Some(libc::B38400),
        57600 => Some(libc::B57600),
        115200 => Some(libc::B115200),
        _ => None,
    }
}

fn tty_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/dev/{path}")
    }
}

fn configure_termios(term: &mut libc::termios, local: bool, baud: u32) {
    term.c_cflag &= !(libc::PARENB | libc::CSTOPB | libc::CSIZE);
    term.c_cflag |= libc::CS8 | libc::CREAD;
    if local {
        term.c_cflag |= libc::CLOCAL;
    } else {
        term.c_cflag &= !libc::CLOCAL;
    }
    term.c_lflag |= libc::ICANON | libc::ECHO | libc::ECHOE | libc::ISIG | libc::IEXTEN;
    term.c_iflag |= libc::ICRNL | libc::IXON;
    term.c_oflag |= libc::OPOST | libc::ONLCR;
    if let Some(speed) = speed(baud) {
        // SAFETY: `term` is a valid termios for these settings.
        unsafe {
            libc::cfsetispeed(term, speed);
            libc::cfsetospeed(term, speed);
        }
    }
}

fn open_tty(path: &str, local: bool, baud: u32) -> Result<(), String> {
    let path = tty_path(path);
    let tty = CString::new(path.as_str()).map_err(|_| "tty path contains NUL".to_string())?;
    let pid = unsafe { libc::getpid() };
    // SAFETY: `tty` is a valid C string; the descriptor is checked below.
    let fd = unsafe { libc::open(tty.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return Err(format!("{path}: {}", io::Error::last_os_error()));
    }
    // SAFETY: a fresh session may fail only when this process is already the
    // session leader. In that case, drop any inherited controlling tty first:
    // TIOCSCTTY would otherwise fail even for the tty we are about to own.
    if unsafe { libc::setsid() } < 0 {
        if unsafe { libc::getsid(0) } != pid {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(format!("setsid: {error}"));
        }
        if let Ok(old_tty) = CString::new("/dev/tty") {
            // SAFETY: the path is NUL-terminated; a missing old tty is fine.
            let old_fd = unsafe { libc::open(old_tty.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
            if old_fd >= 0 {
                // TIOCNOTTY sends SIGHUP to the old foreground group, which may
                // include this process; ignore it while dropping the tty.
                let previous = unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
                unsafe {
                    libc::ioctl(old_fd, libc::TIOCNOTTY);
                    libc::close(old_fd);
                    libc::signal(libc::SIGHUP, previous);
                }
            }
        }
    }
    // SAFETY: `fd` is an open terminal and this process is now its session
    // leader. force=1 steals an already-open console tty.
    if unsafe { libc::ioctl(fd, libc::TIOCSCTTY, 1) } != 0 {
        let error = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(format!("TIOCSCTTY: {error}"));
    }
    for target in 0..=2 {
        if fd != target {
            // SAFETY: dup2 duplicates an open descriptor onto stdio.
            if unsafe { libc::dup2(fd, target) } < 0 {
                let error = io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(format!("dup2: {error}"));
            }
        }
    }
    if fd > 2 {
        // SAFETY: the descriptor has been duplicated.
        unsafe { libc::close(fd) };
    }
    // SAFETY: the process is a session leader and fd 0 is its controlling tty.
    if unsafe { libc::tcsetpgrp(0, pid) } != 0 {
        return Err(format!("tcsetpgrp: {}", io::Error::last_os_error()));
    }

    let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
    // SAFETY: fd 0 is the tty opened above.
    if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
        return Err(format!("tcgetattr: {}", io::Error::last_os_error()));
    }
    configure_termios(&mut term, local, baud);
    // SAFETY: the settings were read from the same terminal.
    if unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) } != 0 {
        return Err(format!("tcsetattr: {}", io::Error::last_os_error()));
    }
    Ok(())
}

fn prompt_username() -> io::Result<String> {
    if let Ok(issue) = fs::read_to_string("/etc/issue") {
        print!("{issue}");
    }
    print!("rinit login: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn report(program: &str, message: &str) {
    eprintln!("{program}: {message}");
    if let Ok(mut kmsg) = OpenOptions::new().append(true).open("/dev/kmsg") {
        let _ = writeln!(kmsg, "{program}: {message}");
    }
}

fn run(args: &[String], program: &str) -> ExitCode {
    if args.iter().any(|arg| arg == "--help") {
        println!("Usage: rgetty [-L] [-l LOGIN] [-h HOST] [-n] TTY [BAUD [TERM]]");
        return ExitCode::from(EXIT_OK);
    }
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(error) => return ExitCode::from(usage_error(program, &error)),
    };
    if let Err(error) = open_tty(&options.tty, options.local, options.baud) {
        report(program, &error);
        return ExitCode::from(1);
    }
    if let Some(term) = &options.term {
        // SAFETY: the process is single-threaded here and has not exec'd yet.
        unsafe { std::env::set_var("TERM", term) };
    }
    let mut username = String::new();
    if !options.no_prompt {
        for _ in 0..3 {
            match prompt_username() {
                Ok(value) if !value.is_empty() => {
                    username = value;
                    break;
                }
                Ok(_) => continue,
                Err(error) => {
                    report(program, &error.to_string());
                    return ExitCode::from(1);
                }
            }
        }
    }
    let mut command = Command::new(&options.login);
    if let Some(host) = &options.host {
        command.args(["-h", host]);
    }
    command.arg("--");
    if !username.is_empty() {
        command.arg(&username);
    }
    let error = command.exec();
    report(
        program,
        &format!("cannot execute {}: {error}", options.login),
    );
    ExitCode::from(1)
}

fn main() -> ExitCode {
    rinit_tools::restore_sigpipe();
    let program = program_name(std::env::args().next().as_deref(), "rgetty");
    let args: Vec<String> = std::env::args().skip(1).collect();
    run(&args, &program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_unit_invocation() {
        let options = parse_args(&[
            "-L".to_string(),
            "ttyAMA0".to_string(),
            "115200".to_string(),
            "vt100".to_string(),
        ])
        .expect("parse");
        assert!(options.local);
        assert_eq!(options.tty, "ttyAMA0");
        assert_eq!(options.baud, 115200);
        assert_eq!(options.term.as_deref(), Some("vt100"));
        assert_eq!(options.login, "/bin/login");
    }

    #[test]
    fn rejects_missing_tty_and_bad_baud() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&["ttyAMA0".to_string(), "fast".to_string()]).is_err());
        assert!(parse_args(&["--bad".to_string()]).is_err());
        assert_eq!(parse_baud("9600"), Ok(9600));
        assert!(
            parse_args(&[
                "ttyS0".to_string(),
                "115200".to_string(),
                "vt100".to_string(),
                "extra".to_string(),
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_login_program_host_and_no_prompt() {
        let options = parse_args(&[
            "-n".to_string(),
            "-l".to_string(),
            "/bin/rlogin".to_string(),
            "-h".to_string(),
            "host".to_string(),
            "ttyS0".to_string(),
            "9600".to_string(),
        ])
        .expect("parse");
        assert!(options.no_prompt);
        assert_eq!(options.login, "/bin/rlogin");
        assert_eq!(options.host.as_deref(), Some("host"));
        assert_eq!(options.baud, 9600);
        assert_eq!(options.term, None);
    }

    #[test]
    fn tty_paths_are_anchored_under_dev() {
        assert_eq!(tty_path("ttyAMA0"), "/dev/ttyAMA0");
        assert_eq!(tty_path("/dev/ttyS0"), "/dev/ttyS0");
    }

    #[test]
    fn termios_configuration_sets_8n1_and_local_mode() {
        let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
        configure_termios(&mut term, true, 115200);
        assert_eq!(term.c_cflag & libc::CSIZE, libc::CS8);
        assert_eq!(term.c_cflag & libc::PARENB, 0);
        assert_eq!(term.c_cflag & libc::CSTOPB, 0);
        assert_ne!(term.c_cflag & libc::CREAD, 0);
        assert_ne!(term.c_cflag & libc::CLOCAL, 0);
        assert_ne!(term.c_lflag & libc::ECHO, 0);
        assert_ne!(term.c_lflag & libc::ICANON, 0);
        configure_termios(&mut term, false, 9600);
        assert_eq!(term.c_cflag & libc::CLOCAL, 0);
    }

    #[test]
    fn speed_table_covers_common_bauds() {
        for (baud, expected) in [
            (1200, libc::B1200),
            (2400, libc::B2400),
            (4800, libc::B4800),
            (9600, libc::B9600),
            (19200, libc::B19200),
            (38400, libc::B38400),
            (57600, libc::B57600),
            (115200, libc::B115200),
        ] {
            assert_eq!(speed(baud), Some(expected));
        }
        assert_eq!(speed(12345), None);
    }

    #[test]
    fn run_handles_help() {
        assert_eq!(
            run(&["--help".to_string()], "rgetty"),
            ExitCode::from(EXIT_OK)
        );
    }
}
