//! Credentials: the syscalls a privilege drop needs (plan §7.4).
//!
//! Plan §7.4 fixes the *order* of these calls and `rinit_core::unit::sandbox`
//! owns that plan; this module owns the calls themselves. Every wrapper records
//! its own operation name, so a failure between `fork` and `exec` can be
//! reported as *which* step failed (`setuid` failed: EPERM) rather than as a
//! nameless child failure - the difference between a diagnosable unit and a
//! mystery in `rctl status`.
//!
//! All of these are no-ops for a process that is already what it asks to be,
//! which is what lets the host tests run them unprivileged: `setuid` to one's
//! own uid and `setgid` to one's own gid are allowed for an unprivileged
//! process; changing to anything else is not, and correctly fails closed.

use crate::error::SyscallError;

/// `prctl(PR_SET_NO_NEW_PRIVS, 1)`.
///
/// After this no `execve` can gain privileges - not through a setuid binary,
/// not through file capabilities - for this process or anything it runs. It is
/// irreversible, needs no privilege to set, and is therefore the first step of
/// plan §7.4's order.
pub fn set_no_new_privs() -> Result<(), SyscallError> {
    // SAFETY: `prctl` reads five scalar arguments here; PR_SET_NO_NEW_PRIVS
    // uses the second as the value and ignores the rest.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("prctl(PR_SET_NO_NEW_PRIVS)"))
    }
}

/// `setgid(2)`: become `gid`'s group.
///
/// Must happen while rinit is still privileged (plan §7.4), so it precedes
/// [`set_uid`].
pub fn set_gid(gid: libc::gid_t) -> Result<(), SyscallError> {
    // SAFETY: `setgid` takes a scalar and cannot touch memory.
    let rc = unsafe { libc::setgid(gid) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("setgid"))
    }
}

/// `setgroups(2)`: replace the supplementary group list.
///
/// This is where rinit *diverges from the letter of plan §7.4*, which names
/// `initgroups(3)`: that call is NSS-backed, so in a static build it wants
/// `libnss_files.so` at run time - the very dependency §7.4 rules out for
/// `User=` lookups, and one a 2 MB initramfs does not carry. The list is built
/// from `/etc/group` by `rinit_core::users::supplementary_gids` instead, which
/// computes the same set, and applied here.
///
/// An empty `gids` is meaningful: it clears the list, which is what a service
/// dropped to an unprivileged user wants (it must not keep root's groups).
pub fn set_groups(gids: &[libc::gid_t]) -> Result<(), SyscallError> {
    // SAFETY: `gids` is a valid slice, and `len` matches it. With `len == 0`
    // the kernel does not dereference the pointer at all.
    let rc = unsafe { libc::setgroups(gids.len(), gids.as_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("setgroups"))
    }
}

/// `setuid(2)`: become `uid`. The last step of the drop, and the point of no
/// return (plan §7.4).
pub fn set_uid(uid: libc::uid_t) -> Result<(), SyscallError> {
    // SAFETY: `setuid` takes a scalar and cannot touch memory.
    let rc = unsafe { libc::setuid(uid) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("setuid"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscall::process;

    /// `no_new_privs` needs no privilege and cannot fail for a normal process.
    /// Setting it here is safe for the rest of the test binary: nothing else in
    /// it asks for a privilege, which is the whole point of the flag.
    #[test]
    fn no_new_privs_can_always_be_set() {
        set_no_new_privs().expect("PR_SET_NO_NEW_PRIVS needs no privileges");
    }

    /// Becoming the user you already are is allowed - and is what makes the
    /// drop testable without root.
    #[test]
    fn becoming_your_own_user_succeeds() {
        set_uid(process::geteuid()).expect("setuid to one's own uid");
        set_gid(getgid_for_test()).expect("setgid to one's own gid");
    }

    /// Changing to a user you are not is refused. Which errno depends on who is
    /// running the test: as root it succeeds (root may become anyone), so the
    /// assertion only applies to the unprivileged case, and says so.
    #[test]
    fn changing_to_another_user_fails_closed() {
        if process::geteuid() == 0 {
            return; // root is allowed to do this; covered in the guest instead.
        }
        let err = set_uid(0).expect_err("an unprivileged process cannot become root");
        assert_eq!(err.errno(), libc::EPERM);
        assert_eq!(err.op(), "setuid");
    }

    /// Changing the group list needs `CAP_SETGID`, which is the reason the
    /// group step precedes `setuid` in the plan (plan §7.4).
    #[test]
    fn changing_the_group_list_needs_privilege() {
        if process::geteuid() == 0 {
            return; // root has CAP_SETGID; the guest covers that path.
        }
        let err = set_groups(&[]).expect_err("an unprivileged process cannot set groups");
        assert_eq!(err.errno(), libc::EPERM);
        assert_eq!(err.op(), "setgroups");
    }

    /// An empty list is a call, not a skipped one: as root it must succeed.
    #[test]
    fn an_empty_group_list_is_accepted_by_root() {
        if process::geteuid() != 0 {
            return;
        }
        set_groups(&[]).expect("root may clear the group list");
    }

    /// The gid of the running test, for the "own gid" case above.
    fn getgid_for_test() -> libc::gid_t {
        // SAFETY: `getgid` takes no arguments and cannot fail.
        unsafe { libc::getgid() }
    }
}
