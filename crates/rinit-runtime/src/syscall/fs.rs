//! Filesystem and credential wrappers the notify socket needs (plan §1.3(12)).
//!
//! Only the pieces with no safe `std` equivalent live here: `umask(2)`,
//! `chmod(2)`, `chown(2)`, and the `getpwnam`/`getgrnam` lookups that turn a
//! unit's `User=`/`Group=` into ids. Directory creation and unlinking use the
//! safe `std::fs` API at the call site.

use crate::error::SyscallError;
use crate::syscall::cstring;

/// Set the process file-creation mask, returning the previous one.
///
/// Used to close the window between `bind` and `chmod` (plan §1.3(12)): binding
/// with `umask(0o117)` makes the new socket `0660` from the moment it exists, so
/// there is no instant where it is more permissive than intended.
pub fn umask(mask: libc::mode_t) -> libc::mode_t {
    // SAFETY: `umask` takes a scalar and has no failure mode.
    unsafe { libc::umask(mask) }
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
/// taken as an id, matching systemd. Otherwise the name is looked up in the
/// system user database.
///
/// # Errors
/// [`SyscallError`] with `ENOENT` when a non-numeric name does not exist.
/// Defaulting a typo to root would silently hand a service root's socket.
pub fn resolve_uid(user: &str) -> Result<libc::uid_t, SyscallError> {
    if user.is_empty() {
        return Ok(0);
    }
    if let Ok(id) = user.parse::<libc::uid_t>() {
        return Ok(id);
    }
    let name = cstring("user name", user)?;
    // SAFETY: `getpwnam` returns either null or a pointer to a static `passwd`
    // whose storage stays valid until the next lookup; we copy the uid out
    // immediately and never hold the pointer.
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        Err(SyscallError::new("getpwnam", libc::ENOENT))
    } else {
        // SAFETY: non-null, so it points at a valid `passwd`.
        Ok(unsafe { (*pw).pw_uid })
    }
}

/// Resolve `Group=` to a gid. Mirrors [`resolve_uid`].
///
/// # Errors
/// [`SyscallError`] with `ENOENT` when a non-numeric name does not exist.
pub fn resolve_gid(group: &str) -> Result<libc::gid_t, SyscallError> {
    if group.is_empty() {
        return Ok(0);
    }
    if let Ok(id) = group.parse::<libc::gid_t>() {
        return Ok(id);
    }
    let name = cstring("group name", group)?;
    // SAFETY: as `resolve_uid`, with `getgrnam`/`group`.
    let gr = unsafe { libc::getgrnam(name.as_ptr()) };
    if gr.is_null() {
        Err(SyscallError::new("getgrnam", libc::ENOENT))
    } else {
        // SAFETY: non-null, so it points at a valid `group`.
        Ok(unsafe { (*gr).gr_gid })
    }
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
