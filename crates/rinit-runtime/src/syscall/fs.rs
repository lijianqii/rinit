//! Filesystem and credential wrappers the notify socket needs (plan §1.3(12)).
//!
//! Only the pieces with no safe `std` equivalent live here: `umask(2)`,
//! `chmod(2)`, `chown(2)`, `pipe2(2)`, `fcntl(2)` and the lookups that turn a
//! unit's `User=`/`Group=` into ids. Directory creation and unlinking use the
//! safe `std::fs` API at the call site.

use std::os::fd::{FromRawFd, OwnedFd};

use rinit_core::users;

use crate::error::SyscallError;
use crate::syscall::cstring;

/// The user database read to resolve a `User=` name.
const PASSWD: &str = "/etc/passwd";

/// The group database read to resolve a `Group=` name.
const GROUP: &str = "/etc/group";

/// Set the process file-creation mask, returning the previous one.
///
/// Used to close the window between `bind` and `chmod` (plan §1.3(12)): binding
/// with `umask(0o117)` makes the new socket `0660` from the moment it exists, so
/// there is no instant where it is more permissive than intended.
pub fn umask(mask: libc::mode_t) -> libc::mode_t {
    // SAFETY: `umask` takes a scalar and has no failure mode.
    unsafe { libc::umask(mask) }
}

/// Set or clear `O_NONBLOCK` on a descriptor.
///
/// A pipe needs its *read* end non-blocking for the async reactor while the
/// *write* end stays blocking, so a child that fills the pipe blocks instead of
/// silently losing output. `pipe2` sets flags on both ends, hence this.
pub fn set_nonblocking(fd: libc::c_int, nonblocking: bool) -> Result<(), SyscallError> {
    // SAFETY: `fcntl` with F_GETFL/F_SETFL reads and writes the descriptor's
    // status flags; both calls take only scalars.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(SyscallError::last_os_error("fcntl(F_GETFL)"));
    }
    let updated = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, updated) } < 0 {
        return Err(SyscallError::last_os_error("fcntl(F_SETFL)"));
    }
    Ok(())
}

/// Create a pipe, returning `(read, write)`, with `O_CLOEXEC` on both ends.
///
/// `O_CLOEXEC` matters: the ends are `dup2`'d onto the child's `stdout`/
/// `stderr` (which clears the flag on *those* descriptors), and rinit must not
/// leak its own copies into any other service it starts.
pub fn pipe() -> Result<(OwnedFd, OwnedFd), SyscallError> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a valid 2-element out-buffer.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(SyscallError::last_os_error("pipe2"));
    }
    // SAFETY: both descriptors are fresh from `pipe2` and are now owned by the
    // `OwnedFd`s, which close them on drop.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// `read(2)` into `buf`, returning the byte count (`0` means end-of-file).
///
/// A blocking read on a blocking descriptor, or an `EAGAIN` on a non-blocking
/// one; the caller decides which it wants. Used by the log pump, which reads
/// service output from a non-blocking pipe.
pub fn read(fd: libc::c_int, buf: &mut [u8]) -> Result<usize, SyscallError> {
    // SAFETY: `buf` is a valid writable slice of the length passed in.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(SyscallError::last_os_error("read"))
    } else {
        Ok(n as usize)
    }
}

/// `chmod(2)` on a path.
pub fn chmod(path: &str, mode: libc::mode_t) -> Result<(), SyscallError> {
    let path = cstring("chmod path", path)?;
    // SAFETY: `path` is a valid, NUL-terminated C string.
    let rc = unsafe { libc::chmod(path.as_ptr(), mode) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("chmod"))
    }
}

/// `chown(2)` on a path.
pub fn chown(path: &str, uid: libc::uid_t, gid: libc::gid_t) -> Result<(), SyscallError> {
    let path = cstring("chown path", path)?;
    // SAFETY: `path` is a valid, NUL-terminated C string.
    let rc = unsafe { libc::chown(path.as_ptr(), uid, gid) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("chown"))
    }
}

/// Whether the current process has `CAP_CHOWN` by virtue of being root.
///
/// Used to skip `chown` on a development host: an unprivileged process cannot
/// hand a socket to another user, and rinit is only ever non-root in tests.
pub fn is_root() -> bool {
    // SAFETY: `geteuid` takes no arguments and never fails.
    unsafe { libc::geteuid() == 0 }
}

/// Resolve `User=` to a uid.
///
/// An empty value means the default (`root`, uid 0). A purely numeric value is
/// taken as an id, matching systemd. Otherwise the name is looked up in
/// `/etc/passwd`.
///
/// The lookup reads the file rather than calling `getpwnam`: rinit is static,
/// and a static `getpwnam` needs the NSS `libnss_files.so` at run time, which
/// the initramfs does not have (see `rinit_core::users`). A name that is only
/// resolvable through NSS - an LDAP or systemd-homed user - therefore does not
/// resolve here. That is deliberate: on a booted system rinit must depend on
/// nothing it cannot read from the image it booted.
///
/// # Errors
/// [`SyscallError`] with `ENOENT` when a non-numeric name does not exist.
/// Defaulting a typo to root would silently hand a service root's socket.
pub fn resolve_uid(user: &str) -> Result<libc::uid_t, SyscallError> {
    let user = user.trim();
    if user.is_empty() {
        return Ok(0);
    }
    if let Ok(id) = user.parse::<libc::uid_t>() {
        return Ok(id);
    }
    let body = read_database(PASSWD)?;
    users::passwd_uid(&body, user).ok_or_else(|| SyscallError::new("User= lookup", libc::ENOENT))
}

/// Resolve `Group=` to a gid. Mirrors [`resolve_uid`], against `/etc/group`.
///
/// # Errors
/// [`SyscallError`] with `ENOENT` when a non-numeric name does not exist.
pub fn resolve_gid(group: &str) -> Result<libc::gid_t, SyscallError> {
    let group = group.trim();
    if group.is_empty() {
        return Ok(0);
    }
    if let Ok(id) = group.parse::<libc::gid_t>() {
        return Ok(id);
    }
    let body = read_database(GROUP)?;
    users::group_gid(&body, group).ok_or_else(|| SyscallError::new("Group= lookup", libc::ENOENT))
}

/// Read a name/id database, mapping an I/O failure to its `errno`.
fn read_database(path: &'static str) -> Result<String, SyscallError> {
    std::fs::read_to_string(path).map_err(|e| SyscallError::from_io(path, &e))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A `chmod` through the wrapper must land on the file, not just return 0.
    #[test]
    fn chmod_sets_the_mode() {
        let path = std::env::temp_dir().join(format!("rinit-fs-{}", std::process::id()));
        std::fs::write(&path, b"x").expect("write temp file");
        let path_str = path.to_str().unwrap();

        chmod(path_str, 0o600).expect("chmod");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn chmod_reports_enoent_for_a_missing_path() {
        let err = chmod("/definitely/not/here/sock", 0o600).expect_err("must fail");
        assert_eq!(err.errno(), libc::ENOENT);
    }

    /// `umask` returns the *previous* mask, which is what makes save-and-restore
    /// possible; corrupting it would leak into every later file rinit creates.
    #[test]
    fn umask_returns_the_previous_mask() {
        let original = umask(0o022);
        let restored = umask(original);
        assert_eq!(restored, 0o022, "umask must return what it replaced");
        assert_eq!(
            umask(original),
            original,
            "and restoring must be idempotent"
        );
    }

    #[test]
    fn empty_user_and_group_default_to_root() {
        assert_eq!(resolve_uid("").unwrap(), 0);
        assert_eq!(resolve_gid("").unwrap(), 0);
    }

    #[test]
    fn numeric_ids_pass_through() {
        assert_eq!(resolve_uid("1234").unwrap(), 1234);
        assert_eq!(resolve_gid("4321").unwrap(), 4321);
    }

    /// A unit file may pad the value; the id must not change because of it.
    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(resolve_uid(" root ").unwrap(), 0);
        assert_eq!(resolve_gid(" 0 ").unwrap(), 0);
    }

    /// An unreadable database is a normal `ENOENT`, not a panic.
    #[test]
    fn a_missing_database_is_reported_as_enoent() {
        let err = read_database("/definitely/not/here/passwd").expect_err("must fail");
        assert_eq!(err.errno(), libc::ENOENT);
    }

    /// Root always exists on a Unix host, so its lookup must resolve to 0.
    #[test]
    fn the_root_name_resolves_to_zero() {
        assert_eq!(resolve_uid("root").unwrap(), 0);
    }

    /// A name that cannot exist must be an error, never a silent fallback to
    /// root - that would hand a service the wrong socket.
    #[test]
    fn an_unknown_name_is_an_error() {
        let err = resolve_uid("rinit-no-such-user-xyz").expect_err("must fail");
        assert_eq!(err.errno(), libc::ENOENT);
        let err = resolve_gid("rinit-no-such-group-xyz").expect_err("must fail");
        assert_eq!(err.errno(), libc::ENOENT);
    }

    /// `chown` to our own ids must succeed for an unprivileged test too.
    #[test]
    fn chown_to_the_current_ids_succeeds() {
        let path = std::env::temp_dir().join(format!("rinit-chown-{}", std::process::id()));
        std::fs::write(&path, b"x").expect("write temp file");
        // SAFETY: `geteuid`/`getegid` take no arguments.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };

        chown(path.to_str().unwrap(), uid, gid).expect("chown to self");
        std::fs::remove_file(&path).ok();
    }
}
