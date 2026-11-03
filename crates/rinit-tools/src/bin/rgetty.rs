//! `rgetty` - open a terminal and hand it to `rlogin`.

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use rinit_tools::{EXIT_OK, program_name, usage_error, utmp};

struct Options {
    local: bool,
    login: String,
    host: Option<String>,
    no_prompt: bool,
    tty: String,
    baud: u32,
    term: Option<String>,
    timeout: Option<u32>,
    init: Option<String>,
}

fn parse_baud(value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("invalid baud rate '{value}'"))
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        local: false,
        login: "/bin/rlogin".to_string(),
        host: None,
        no_prompt: false,
        tty: String::new(),
        baud: 115200,
        term: None,
        timeout: None,
        init: None,
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
            "-t" | "--timeout" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("-t needs a timeout".to_string());
                };
                options.timeout = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid timeout '{value}'"))?,
                );
            }
            "-I" | "--init-string" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("-I needs an init string".to_string());
                };
                options.init = Some(value.clone());
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
    // Establish a complete baseline; flags left by the previous user must not
    // leak into the next session.
    term.c_lflag = libc::ICANON | libc::ECHO | libc::ECHOE | libc::ISIG | libc::IEXTEN;
    term.c_iflag = libc::ICRNL | libc::IXON;
    term.c_oflag = libc::OPOST | libc::ONLCR;
    term.c_cc = [0; libc::NCCS];
    term.c_cc[libc::VINTR] = 3;
    term.c_cc[libc::VQUIT] = 28;
    term.c_cc[libc::VERASE] = 127;
    term.c_cc[libc::VKILL] = 21;
    term.c_cc[libc::VEOF] = 4;
    term.c_cc[libc::VSTART] = 17;
    term.c_cc[libc::VSTOP] = 19;
    term.c_cc[libc::VSUSP] = 26;
    term.c_cc[libc::VMIN] = 1;
    term.c_cc[libc::VTIME] = 0;
    if let Some(speed) = speed(baud) {
        // SAFETY: `term` is a valid termios for these settings.
        unsafe {
            libc::cfsetispeed(term, speed);
            libc::cfsetospeed(term, speed);
        }
    }
}

fn ignore_job_control_signals() {
    // SAFETY: fixed signal dispositions are process-wide; no handler state.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTOU] {
            libc::signal(signal, libc::SIG_IGN);
        }
    }
}

fn default_job_control_signals() {
    // SAFETY: restore the default dispositions before exec'ing login/shell.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTOU] {
            libc::signal(signal, libc::SIG_DFL);
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
    // Reclaim the device from the previous session before anyone can log in.
    if unsafe { libc::fchown(0, 0, 0) } != 0 {
        return Err(format!("fchown tty: {}", io::Error::last_os_error()));
    }
    if unsafe { libc::fchmod(0, 0o600) } != 0 {
        return Err(format!("fchmod tty: {}", io::Error::last_os_error()));
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
    // Drop typeahead typed before the prompt, including input from the
    // previous session.
    unsafe {
        libc::tcflush(0, libc::TCIOFLUSH);
    }
    Ok(())
}

fn read_username() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn read_username_timeout(seconds: u32) -> io::Result<String> {
    let deadline = Instant::now() + Duration::from_secs(seconds as u64);
    let mut bytes = Vec::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "login timed out"));
        }
        let remaining = deadline
            .duration_since(now)
            .as_millis()
            .min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `descriptor` points at one valid pollfd.
        let ready = unsafe { libc::poll(&mut descriptor, 1, remaining) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "login timed out"));
        }
        let mut byte = [0_u8; 1];
        // SAFETY: fd 0 is the tty and `byte` is a one-byte destination.
        let read = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        if read <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "login input closed",
            ));
        }
        match byte[0] {
            b'\n' => break,
            b'\r' => continue,
            value => bytes.push(value),
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn prompt_username(timeout: Option<u32>) -> io::Result<String> {
    if let Ok(issue) = fs::read_to_string("/etc/issue") {
        print!("{issue}");
    }
    print!("rinit login: ");
    io::stdout().flush()?;
    match timeout {
        Some(seconds) if seconds > 0 => read_username_timeout(seconds),
        _ => read_username(),
    }
}

fn report(program: &str, message: &str) {
    eprintln!("{program}: {message}");
    if let Ok(mut kmsg) = OpenOptions::new().append(true).open("/dev/kmsg") {
        let _ = writeln!(kmsg, "{program}: {message}");
    }
}

fn run(args: &[String], program: &str) -> ExitCode {
    if args.iter().any(|arg| arg == "--help") {
        println!(
            "Usage: rgetty [-L] [-l LOGIN] [-h HOST] [-n] [-t SEC] [-I STRING] TTY [BAUD [TERM]]"
        );
        return ExitCode::from(EXIT_OK);
    }
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(error) => return ExitCode::from(usage_error(program, &error)),
    };
    ignore_job_control_signals();
    if let Err(error) = open_tty(&options.tty, options.local, options.baud) {
        report(program, &error);
        return ExitCode::from(1);
    }
    let getty_pid = unsafe { libc::getpid() };
    if let Err(error) =
        utmp::login_record(getty_pid, &options.tty, "LOGIN", options.host.as_deref())
    {
        report(program, &format!("utmp: {error}"));
    }
    if let Some(init) = &options.init {
        let mut stdout = io::stdout().lock();
        if let Err(error) = stdout
            .write_all(init.as_bytes())
            .and_then(|()| stdout.flush())
        {
            report(program, &error.to_string());
            return ExitCode::from(1);
        }
    }
    if let Some(term) = &options.term {
        // SAFETY: the process is single-threaded here and has not exec'd yet.
        unsafe { std::env::set_var("TERM", term) };
    }
    let mut username = String::new();
    if !options.no_prompt {
        for _ in 0..3 {
            match prompt_username(options.timeout) {
                Ok(value) if !value.is_empty() => {
                    username = value;
                    break;
                }
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                    println!(
                        "\r\nLogin timed out after {} seconds",
                        options.timeout.unwrap_or(0)
                    );
                    return ExitCode::from(1);
                }
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
    // The child owns the foreground process group, so the shell gets the tty
    // signals while this parent waits for the session to end and records it.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            let pid = libc::getpid();
            libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            if libc::tcsetpgrp(0, pid) != 0 {
                return Err(io::Error::last_os_error());
            }
            default_job_control_signals();
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            report(
                program,
                &format!("cannot execute {}: {error}", options.login),
            );
            return ExitCode::from(1);
        }
    };
    let status = child.wait();
    if let Err(error) = utmp::dead_record(child.id() as i32, &options.tty) {
        report(program, &format!("utmp: {error}"));
    }
    match status {
        Ok(status) => ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1),
        ),
        Err(error) => {
            report(program, &format!("wait: {error}"));
            ExitCode::from(1)
        }
    }
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
        assert_eq!(options.login, "/bin/rlogin");
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

        let options = parse_args(&[
            "-t".to_string(),
            "30".to_string(),
            "-I".to_string(),
            "ATZ\\r".to_string(),
            "ttyS0".to_string(),
        ])
        .expect("timeout/init");
        assert_eq!(options.timeout, Some(30));
        assert_eq!(options.init.as_deref(), Some("ATZ\\r"));
        assert!(parse_args(&["-t".to_string(), "bad".to_string()]).is_err());
    }

    #[test]
    fn tty_paths_are_anchored_under_dev() {
        assert_eq!(tty_path("ttyAMA0"), "/dev/ttyAMA0");
        assert_eq!(tty_path("/dev/ttyS0"), "/dev/ttyS0");
    }

    #[test]
    fn termios_configuration_sets_8n1_and_local_mode() {
        let mut term = unsafe { std::mem::zeroed::<libc::termios>() };
        term.c_lflag |= libc::TOSTOP | libc::ECHOCTL;
        term.c_iflag |= libc::PARMRK;
        term.c_oflag |= libc::OCRNL;
        configure_termios(&mut term, true, 115200);
        assert_eq!(term.c_lflag & (libc::TOSTOP | libc::ECHOCTL), 0);
        assert_eq!(term.c_iflag & libc::PARMRK, 0);
        assert_eq!(term.c_oflag & libc::OCRNL, 0);
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
    fn timeout_reader_reads_a_line_and_reports_timeouts() {
        let mut fds = [0_i32; 2];
        // SAFETY: `fds` is writable storage for a pipe pair.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: save and replace stdin for this test only.
        let saved = unsafe { libc::dup(0) };
        assert!(saved >= 0);
        assert_eq!(unsafe { libc::dup2(fds[0], 0) }, 0);
        let input = b"root\n";
        // SAFETY: the write end is open and the buffer is valid.
        assert_eq!(
            unsafe { libc::write(fds[1], input.as_ptr().cast(), input.len()) },
            input.len() as isize
        );
        assert_eq!(read_username_timeout(5).expect("line"), "root");
        assert_eq!(
            read_username_timeout(0).expect_err("timeout").kind(),
            io::ErrorKind::TimedOut
        );
        // SAFETY: restore the original stdin and close the temporary ends.
        unsafe {
            libc::dup2(saved, 0);
            libc::close(saved);
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
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
    fn run_handles_help() {
        assert_eq!(
            run(&["--help".to_string()], "rgetty"),
            ExitCode::from(EXIT_OK)
        );
    }
}
