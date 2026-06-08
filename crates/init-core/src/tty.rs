//! Terminal session spawning for getty-like services.
//!
//! When a service unit has a `tty` configured, the child process
//! is attached to a real terminal with proper session setup:
//!   - Open the TTY device
//!   - Set baud rate and raw-ish terminal attributes
//!   - Create new session (setsid) and set controlling TTY
//!   - dup2 the TTY fd to stdin/stdout/stderr
//!   - exec the target command

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::process::Command;
use tracing::{debug, info, warn};

/// Fork and exec a command attached to a terminal device.
///
/// Returns the child PID. The child becomes a session leader with
/// stdin/stdout/stderr connected to the TTY.
pub fn spawn_terminal(
    path: &str,
    args: &[String],
    tty: &str,
    baud: u32,
) -> Result<libc::pid_t> {
    match unsafe { nix::unistd::fork() }.context("fork for terminal failed")? {
        nix::unistd::ForkResult::Parent { child } => {
            let pid = child.as_raw();
            info!(pid, path = %path, tty = %tty, "terminal session spawned");
            Ok(pid)
        }
        nix::unistd::ForkResult::Child => {
            // Reset signals to default
            for sig in 1..=31 {
                unsafe { libc::signal(sig, libc::SIG_DFL) };
            }

            if let Err(e) = setup_terminal_child(tty, baud) {
                let _ = std::io::Write::write_fmt(
                    &mut std::io::stderr(),
                    format_args!("rinit: terminal setup failed: {}\n", e),
                );
                std::process::exit(1);
            }

            // Run login prompt
    if let Err(e) = crate::login::do_login(0) {
        let _ = std::io::Write::write_fmt(
            &mut std::io::stderr(),
            format_args!("rinit: login failed: {}
", e),
        );
        std::process::exit(1);
    }

            // Close extra fds (but keep 0/1/2 which now point to TTY)
            let maxfd = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
            let limit = if maxfd > 0 { maxfd as i32 } else { 1024 };
            for fd in 3..limit {
                unsafe { libc::close(fd) };
            }

            unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };

            let err = Command::new(path).args(args).exec();
            let _ = std::io::Write::write_fmt(
                &mut std::io::stderr(),
                format_args!("rinit: exec {} failed: {}\n", path, err),
            );
            std::process::exit(127);
        }
    }
}

fn setup_terminal_child(tty: &str, baud: u32) -> Result<()> {
    // Open the TTY device
    let tty_path = if tty.starts_with('/') {
        tty.to_string()
    } else {
        format!("/dev/{}", tty)
    };

    let fd = unsafe {
        let tty_c = std::ffi::CString::new(tty_path.as_str())
            .context("invalid tty path")?;
        let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
        libc::open(tty_c.as_ptr(), flags)
    };

    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("open({})", tty));
    }

    // Set terminal attributes
    let speed = baud_to_speed(baud);
    let mut term: libc::termios = unsafe { std::mem::zeroed() };

    // Get current attributes
    if unsafe { libc::tcgetattr(fd, &mut term) } < 0 {
        let err = std::io::Error::last_os_error();
        warn!(tty, error = %err, "tcgetattr failed, continuing");
    }

    // Set input baud rate
    unsafe {
        libc::cfsetispeed(&mut term, speed);
        libc::cfsetospeed(&mut term, speed);
    }

    // Configure terminal: canonical mode with echo, translate CR→LF
    term.c_iflag |= libc::ICRNL;
    term.c_iflag &= !(libc::IXON | libc::IXOFF);
    term.c_oflag |= libc::ONLCR;
    term.c_lflag |= libc::ICANON | libc::ECHO | libc::ECHOE | libc::ECHOK;
    term.c_lflag &= !libc::ISIG;
    term.c_cc[libc::VMIN] = 1;
    term.c_cc[libc::VTIME] = 0;

    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &term);
    }

    // Create a new session
    nix::unistd::setsid().context("setsid")?;

    // Set this TTY as the controlling terminal
    if unsafe { libc::ioctl(fd, libc::TIOCSCTTY as _, 0) } < 0 {
        warn!("TIOCSCTTY failed (non-fatal)");
    }

    // dup2 the TTY fd to stdin/stdout/stderr
    for target in 0..=2 {
        if fd != target {
            unsafe { libc::dup2(fd, target) };
        }
    }
    // Close the original TTY fd if it's > 2
    if fd > 2 {
        unsafe { libc::close(fd) };
    }

    debug!(tty, baud, "terminal session set up");
    Ok(())
}

fn baud_to_speed(baud: u32) -> libc::speed_t {
    match baud {
        0 => libc::B0,
        50 => libc::B50,
        75 => libc::B75,
        110 => libc::B110,
        134 => libc::B134,
        150 => libc::B150,
        200 => libc::B200,
        300 => libc::B300,
        600 => libc::B600,
        1200 => libc::B1200,
        1800 => libc::B1800,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115200 => libc::B115200,
        230400 => libc::B230400,
        460800 => libc::B460800,
        500000 => libc::B500000,
        576000 => libc::B576000,
        921600 => libc::B921600,
        1000000 => libc::B1000000,
        1152000 => libc::B1152000,
        1500000 => libc::B1500000,
        2000000 => libc::B2000000,
        2500000 => libc::B2500000,
        3000000 => libc::B3000000,
        3500000 => libc::B3500000,
        4000000 => libc::B4000000,
        _ => libc::B115200,
    }
}
