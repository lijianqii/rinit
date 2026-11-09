//! Minimal utmp/wtmp accounting for the console login tools.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const LOGIN_PROCESS: i16 = 6;
const USER_PROCESS: i16 = 7;
const DEAD_PROCESS: i16 = 8;

unsafe extern "C" {
    fn updwtmpx(file: *const libc::c_char, entry: *const libc::utmpx);
}

pub fn login_record(pid: i32, tty: &str, user: &str, host: Option<&str>) -> io::Result<()> {
    record(LOGIN_PROCESS, pid, tty, user, host)
}

pub fn user_record(pid: i32, tty: &str, user: &str, host: Option<&str>) -> io::Result<()> {
    record(USER_PROCESS, pid, tty, user, host)
}

pub fn dead_record(pid: i32, tty: &str) -> io::Result<()> {
    record(DEAD_PROCESS, pid, tty, "", None)
}

/// Count live `USER_PROCESS` records in a utmp file.
///
/// A missing or unreadable file is not fatal to `uptime`; the caller decides
/// whether zero or an error is the better answer.
pub fn user_count(path: &Path) -> io::Result<usize> {
    let data = std::fs::read(path)?;
    let size = std::mem::size_of::<libc::utmpx>();
    let mut count = 0;
    for chunk in data.chunks_exact(size) {
        // Copying the bytes is alignment-safe; reading a `&utmpx` directly
        // from the file buffer would not be.
        let mut entry: libc::utmpx = unsafe { std::mem::zeroed() };
        // SAFETY: `chunk` is exactly `size_of::<utmpx>()` bytes and `entry` is
        // a valid `utmpx` destination.
        unsafe {
            std::ptr::copy_nonoverlapping(
                chunk.as_ptr(),
                (&mut entry as *mut libc::utmpx).cast::<u8>(),
                size,
            );
        }
        if entry.ut_type == USER_PROCESS {
            count += 1;
        }
    }
    Ok(count)
}

fn record(kind: i16, pid: i32, tty: &str, user: &str, host: Option<&str>) -> io::Result<()> {
    record_at(
        Path::new("/var/run/utmp"),
        Path::new("/var/log/wtmp"),
        kind,
        pid,
        tty,
        user,
        host,
    )
}

fn record_at(
    utmp_path: &Path,
    wtmp_path: &Path,
    kind: i16,
    pid: i32,
    tty: &str,
    user: &str,
    host: Option<&str>,
) -> io::Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(utmp_path)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(wtmp_path)?;
    let utmp_name = CString::new(utmp_path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "utmp path contains NUL"))?;
    let wtmp_name = CString::new(wtmp_path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "wtmp path contains NUL"))?;
    let mut entry = unsafe { std::mem::zeroed::<libc::utmpx>() };
    entry.ut_type = kind;
    entry.ut_pid = pid;
    set_field(&mut entry.ut_line, &short_tty(tty));
    set_field(&mut entry.ut_id, &tty_id(tty));
    set_field(&mut entry.ut_user, user);
    if let Some(host) = host {
        set_field(&mut entry.ut_host, host);
    }
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    entry.ut_tv.tv_sec = now as _;
    entry.ut_tv.tv_usec = 0;
    // SAFETY: the path is a valid C string and `entry` is fully initialized.
    unsafe {
        libc::utmpxname(utmp_name.as_ptr());
        libc::setutxent();
        let result = libc::pututxline(&entry);
        libc::endutxent();
        if result.is_null() {
            return Err(io::Error::other("pututxline failed"));
        }
        updwtmpx(wtmp_name.as_ptr(), &entry);
    }
    Ok(())
}

fn short_tty(tty: &str) -> String {
    tty.strip_prefix("/dev/").unwrap_or(tty).to_string()
}

fn tty_id(tty: &str) -> String {
    let short = short_tty(tty);
    let id = if short.starts_with('p') {
        format!("p{}", short.get(3..).unwrap_or(""))
    } else if let Some(rest) = short.strip_prefix("tty") {
        rest.to_string()
    } else {
        short
    };
    id.chars().take(4).collect()
}

fn set_field<const N: usize>(field: &mut [libc::c_char; N], value: &str) {
    let mut written = 0;
    for (slot, byte) in field.iter_mut().zip(value.bytes()) {
        *slot = byte as libc::c_char;
        written += 1;
    }
    if written < N
        && let Some(slot) = field.get_mut(written)
    {
        *slot = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rinit-utmp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    #[test]
    fn tty_names_and_ids_match_utmp_conventions() {
        assert_eq!(short_tty("/dev/ttyAMA0"), "ttyAMA0");
        assert_eq!(tty_id("ttyAMA0"), "AMA0");
        assert_eq!(tty_id("/dev/ttyS0"), "S0");
        assert_eq!(tty_id("pts/1"), "p/1");
    }

    #[test]
    fn a_record_carries_the_expected_fields() {
        let root = temp_root("fields");
        let utmp = root.join("utmp");
        let wtmp = root.join("wtmp");
        record_at(
            &utmp,
            &wtmp,
            USER_PROCESS,
            4242,
            "/dev/ttyAMA0",
            "alice",
            Some("host"),
        )
        .expect("user record");

        let data = std::fs::read(&utmp).expect("utmp");
        let size = std::mem::size_of::<libc::utmpx>();
        let chunk = data.chunks_exact(size).last().expect("one record");
        let mut entry: libc::utmpx = unsafe { std::mem::zeroed() };
        // SAFETY: `chunk` is exactly one utmpx record.
        unsafe {
            std::ptr::copy_nonoverlapping(
                chunk.as_ptr(),
                (&mut entry as *mut libc::utmpx).cast::<u8>(),
                size,
            );
        }
        let field = |bytes: &[libc::c_char]| {
            bytes
                .iter()
                .map(|byte| *byte as u8)
                .take_while(|byte| *byte != 0)
                .collect::<Vec<_>>()
        };
        assert_eq!(entry.ut_type, USER_PROCESS);
        assert_eq!(entry.ut_pid, 4242);
        assert_eq!(field(&entry.ut_line), b"ttyAMA0");
        assert_eq!(field(&entry.ut_id), b"AMA0");
        assert_eq!(field(&entry.ut_user), b"alice");
        assert_eq!(field(&entry.ut_host), b"host");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn writes_utmp_and_wtmp_records() {
        let root = temp_root("record");
        let utmp = root.join("utmp");
        let wtmp = root.join("wtmp");
        record_at(&utmp, &wtmp, LOGIN_PROCESS, 42, "ttyAMA0", "LOGIN", None).expect("login record");
        record_at(
            &utmp,
            &wtmp,
            USER_PROCESS,
            42,
            "ttyAMA1",
            "root",
            Some("console"),
        )
        .expect("user record");
        record_at(&utmp, &wtmp, DEAD_PROCESS, 42, "ttyAMA0", "", None).expect("dead record");
        assert!(std::fs::metadata(&utmp).expect("utmp").len() > 0);
        assert!(std::fs::metadata(&wtmp).expect("wtmp").len() > 0);
        assert_eq!(user_count(&utmp).expect("count"), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
