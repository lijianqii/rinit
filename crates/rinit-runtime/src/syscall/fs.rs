//! Filesystem and credential wrappers the notify socket needs (plan §1.3(12)).
//!
//! Only the pieces with no safe `std` equivalent live here: `umask(2)`,
//! `chmod(2)`, `chown(2)`, `pipe2(2)` and `fcntl(2)`. Directory creation and
//! unlinking use the safe `std::fs` API at the call site, and `User=`/`Group=`
//! resolution is policy in `rinit/src/sandbox.rs`.

use std::os::fd::{FromRawFd, OwnedFd};

use crate::error::SyscallError;
use crate::syscall::cstring;

/// Set the process file-creation mask, returning the previous one.
///
/// Used to close the window between `bind` and `chmod` (plan §1.3(12)): a
/// narrow mask keeps a new socket owner-only from the moment it exists, so
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
