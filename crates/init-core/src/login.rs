//! Minimal login for the native terminal session.
//!
//! Prompts for username and password on the TTY, validates against
//! /etc/passwd (or accepts root:any for development), then sets up
//! the user environment before exec'ing the shell.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::IntoRawFd;
use std::os::unix::io::FromRawFd;
use tracing::info;

extern "C" {
    fn crypt(key: *const libc::c_char, salt: *const libc::c_char) -> *mut libc::c_char;
}

/// Result of a successful login.
pub struct LoginResult {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

/// Run the login prompt on the given TTY file descriptor.
/// Returns the login result if successful.
///
/// Call this after terminal setup but before exec in the child process.
pub fn do_login(fd: libc::c_int) -> Result<LoginResult, String> {
    // Create BufferedReader/Writer from raw fd
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut reader = BufReader::new(file.try_clone().map_err(|e| e.to_string())?);
    let mut writer = file;

    // Prompt for username
    let _ = write!(writer, "\nrinit login: ");
    let _ = writer.flush();

    let mut username = String::new();
    reader.read_line(&mut username).map_err(|e| e.to_string())?;
    let username = username.trim().to_string();

    if username.is_empty() {
        return Err("empty username".into());
    }

    // Prompt for password (turn off echo)
    let _ = write!(writer, "Password: ");
    let _ = writer.flush();

    // Disable echo
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    unsafe { libc::tcgetattr(0, &mut term) };
    let old_lflag = term.c_lflag;
    term.c_lflag &= !libc::ECHO;
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) };

    let mut password = String::new();
    reader.read_line(&mut password).map_err(|e| e.to_string())?;
    let _password = password.trim().to_string();

    // Restore echo
    term.c_lflag = old_lflag;
    unsafe { libc::tcsetattr(0, libc::TCSANOW, &term) };
    let _ = writeln!(writer);

    // Validate (dev mode: accept root with any password)
    let valid = validate(&username, &_password);

    if !valid {
        let _ = writeln!(writer, "Login incorrect");
        return Err("invalid credentials".into());
    }

    // Get user info
    let mut user = get_user_info(&username).unwrap_or_else(|| UserInfo {
        uid: 0,
        gid: 0,
        home: "/root".into(),
        shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
    });
    if user.shell.is_empty() {
        user.shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    }
    if user.home.is_empty() {
        user.home = "/root".to_string();
    }

    info!(user = %username, uid = user.uid, "login successful");

    // Set up environment
    std::env::set_var("HOME", &user.home);
    std::env::set_var("USER", &username);
    std::env::set_var("LOGNAME", &username);
    std::env::set_var("SHELL", &user.shell);
    std::env::set_var("PATH", "/bin:/sbin:/usr/bin:/usr/sbin");
    std::env::set_var("TERM", "vt102");
    std::env::set_var("HOSTNAME", "rinit");

    // Try to set UID/GID (only works as root)
    unsafe {
        libc::setgid(user.gid);
        libc::setuid(user.uid);
    }

    // Prevent File Drop from closing fd 0 (stdin).
    let _ = writer.into_raw_fd();
    std::mem::forget(reader);

    Ok(LoginResult {
        username,
        uid: user.uid,
        gid: user.gid,
        home: user.home,
        shell: user.shell,
    })
}

fn validate(username: &str, password: &str) -> bool {
    let shadow = match std::fs::read_to_string("/etc/shadow") {
        Ok(s) => s,
        Err(_) => return false,
    };

    for line in shadow.lines() {
        let fields: Vec<&str> = line.splitn(3, ':').collect();
        if fields.len() >= 2 && fields[0] == username {
            let stored_hash = fields[1];
            if stored_hash.is_empty() {
                return true;
            }
            let entered = unsafe {
                let pw = std::ffi::CString::new(password).unwrap();
                let salt = std::ffi::CString::new(stored_hash).unwrap();
                let result = crypt(pw.as_ptr(), salt.as_ptr());
                if result.is_null() {
                    return false;
                }
                std::ffi::CStr::from_ptr(result).to_string_lossy().into_owned()
            };
            return entered == stored_hash;
        }
    }
    false
}

struct UserInfo {
    uid: u32,
    gid: u32,
    home: String,
    shell: String,
}

fn get_user_info(username: &str) -> Option<UserInfo> {
    if let Ok(content) = std::fs::read_to_string("/etc/passwd") {
        for line in content.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() >= 7 && fields[0] == username {
                return Some(UserInfo {
                    uid: fields[2].parse().unwrap_or(0),
                    gid: fields[3].parse().unwrap_or(0),
                    home: fields[5].to_string(),
                    shell: fields[6].to_string(),
                });
            }
        }
    }
    None
}
