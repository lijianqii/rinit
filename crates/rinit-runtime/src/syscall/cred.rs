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
//!
//! Capabilities (plan §7.4) straddle the `setuid` boundary, and the kernel is
//! what puts them there. `cap_emulate_setxuid` (`security/commoncap.c`) clears
//! the ambient set on *every* root-to-non-root `setuid`, whatever
//! `PR_SET_KEEPCAPS` says - so the ambient set cannot be established before the
//! uid change and be expected to survive it. What must happen early is what
//! needs `CAP_SETPCAP`: narrowing the bounding set, and growing the inheritable
//! set, which `cap_capset` only allows within `old_inheritable | old_bset` - so
//! it has to happen *before* the bounding set shrinks, or a capability that is
//! being narrowed away could never be inherited. `PR_SET_KEEPCAPS` keeps the
//! permitted set across the uid change, which is what the later
//! `PR_CAP_AMBIENT_RAISE` needs alongside the inheritable bit.

use std::collections::HashMap;
use std::sync::LazyLock;

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

/// Linux capability names to their bit numbers (plan §7.4).
///
/// Plan §7.4 puts this table here rather than in `rinit-core` on purpose: the
/// model has no `std`, and a table of names is not something a parser needs to
/// know. `rinit_core::unit::config::Cap` validates and normalises the *name*
/// (so `CAP_SYS_ADMIN`, `cap_sys_admin` and `sys_admin` are one key) and this is
/// the only place a name becomes a number.
///
/// The numbers are the kernel's `include/uapi/linux/capability.h` - capability
/// numbers are ABI, so they are written out rather than derived. `CAP_` is
/// stripped and the name lower-cased to match [`rinit_core::unit::config::Cap`].
static CAPABILITY_NAMES: LazyLock<HashMap<&'static str, u32>> = LazyLock::new(|| {
    HashMap::from([
        ("chown", 0),
        ("dac_override", 1),
        ("dac_read_search", 2),
        ("fowner", 3),
        ("fsetid", 4),
        ("kill", 5),
        ("setgid", 6),
        ("setuid", 7),
        ("setpcap", 8),
        ("linux_immutable", 9),
        ("net_bind_service", 10),
        ("net_broadcast", 11),
        ("net_admin", 12),
        ("net_raw", 13),
        ("ipc_lock", 14),
        ("ipc_owner", 15),
        ("sys_module", 16),
        ("sys_rawio", 17),
        ("sys_chroot", 18),
        ("sys_ptrace", 19),
        ("sys_pacct", 20),
        ("sys_admin", 21),
        ("sys_boot", 22),
        ("sys_nice", 23),
        ("sys_resource", 24),
        ("sys_time", 25),
        ("sys_tty_config", 26),
        ("mknod", 27),
        ("lease", 28),
        ("audit_write", 29),
        ("audit_control", 30),
        ("setfcap", 31),
        ("mac_override", 32),
        ("mac_admin", 33),
        ("syslog", 34),
        ("wake_alarm", 35),
        ("block_suspend", 36),
        ("audit_read", 37),
        ("perfmon", 38),
        ("bpf", 39),
        ("checkpoint_restore", 40),
    ])
});

/// The bit number of a normalised capability name, or `None` if the kernel has
/// no such capability.
///
/// `None` is a fail-closed signal for the caller (plan §7.4): a unit that names
/// a capability this build does not know is not started with a subset of what
/// it asked for.
pub fn capability_number(name: &str) -> Option<u32> {
    CAPABILITY_NAMES.get(name).copied()
}

/// The highest capability number the kernel ABI defines (64 bits, two words).
///
/// Nothing in this build resolves to a number near this - the table above stops
/// at 40 - but [`set_bounding_set`] has to consider every number that could be
/// set, not just the ones this build can name.
const MAX_CAPABILITY: u32 = 63;

/// `prctl(PR_CAPBSET_DROP)` for every capability outside `keep`.
///
/// The bounding set is what a *root* process is left with at `exec`: the kernel
/// recomputes `permitted = bset | inheritable` for a root caller
/// (`handle_privileged_root` in `security/commoncap.c`). So dropping every bit
/// here is what makes an empty `CapabilityBoundingSet=` mean "no capabilities
/// after `exec`" even though the service stays root.
///
/// Needs `CAP_SETPCAP`, which is why plan §7.4 requires it before `setuid`. An
/// unprivileged caller gets `EPERM` on the first capability - fail-closed, and
/// the reason a caps-bearing unit cannot start under `--dev`.
pub fn set_bounding_set(keep: &[u32]) -> Result<(), SyscallError> {
    for cap in 0..=MAX_CAPABILITY {
        if keep.contains(&cap) {
            continue;
        }
        // SAFETY: `prctl` reads scalar arguments only here; PR_CAPBSET_DROP
        // uses the second as the capability number and ignores the rest.
        let rc = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) };
        if rc == 0 {
            continue;
        }
        let err = SyscallError::last_os_error("prctl(PR_CAPBSET_DROP)");
        // `cap_prctl_drop` checks `CAP_SETPCAP` first and `cap_valid` second, so
        // an unprivileged caller sees `EPERM` and a privileged one sees
        // `EINVAL` past the last capability the kernel knows - which is the loop
        // being finished, not a failure.
        if err.errno() == libc::EINVAL {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}

/// `prctl(PR_SET_KEEPCAPS, 1)`: keep the permitted set across the coming
/// `setuid`.
///
/// Without it `cap_emulate_setxuid` clears permitted along with effective, and
/// the ambient raise after the drop has nothing to raise, because
/// `PR_CAP_AMBIENT_RAISE` requires the capability in *both* permitted and
/// inheritable.
pub fn keep_capabilities() -> Result<(), SyscallError> {
    // SAFETY: `prctl` reads scalar arguments only; PR_SET_KEEPCAPS uses the
    // second as the boolean value.
    let rc = unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("prctl(PR_SET_KEEPCAPS)"))
    }
}

/// Add `caps` to the inheritable set, leaving permitted and effective alone.
///
/// Must run *before* [`set_bounding_set`]: `cap_capset` only accepts an
/// inheritable bit that is inside `old_inheritable | old_bset`, so once the
/// bounding set has been narrowed, a capability that was narrowed away can no
/// longer be inherited - and an ambient capability has to be inheritable first.
///
/// glibc has no wrapper for `capget`/`capset`, so the kernel ABI is spelled out
/// here: version 3 of the header, two 32-bit words of sets.
pub fn set_inheritable(caps: &[u32]) -> Result<(), SyscallError> {
    let mut data = capability_sets()?;
    for cap in caps {
        let index = (cap / 32) as usize;
        let bit = 1u32 << (cap % 32);
        let Some(word) = data.get_mut(index) else {
            // Unreachable through this crate's table, but a number outside the
            // ABI is a caller bug, not something to silently ignore.
            return Err(SyscallError::new("capset", libc::EINVAL));
        };
        word.inheritable |= bit;
    }
    write_capability_sets(&data)
}

/// Raise `caps` in the ambient set, or clear the set when `caps` is empty.
///
/// This is the one capability step that must run *after* `setuid` (see this
/// module's header). Empty is meaningful: `AmbientCapabilities=` with no names
/// asks for no ambient capabilities, and a forked child would otherwise inherit
/// whatever ambient set rinit itself happened to have.
pub fn set_ambient(caps: &[u32]) -> Result<(), SyscallError> {
    if caps.is_empty() {
        // SAFETY: `prctl` reads scalar arguments only; CLEAR_ALL takes no
        // further arguments, which is why the kernel rejects them if given.
        let rc = unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            )
        };
        return if rc == 0 {
            Ok(())
        } else {
            Err(SyscallError::last_os_error(
                "prctl(PR_CAP_AMBIENT_CLEAR_ALL)",
            ))
        };
    }
    for cap in caps {
        // SAFETY: `prctl` reads scalar arguments only; PR_CAP_AMBIENT_RAISE uses
        // the third as the capability number.
        let rc = unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_RAISE,
                *cap as libc::c_ulong,
                0,
                0,
            )
        };
        if rc != 0 {
            return Err(SyscallError::last_os_error("prctl(PR_CAP_AMBIENT_RAISE)"));
        }
    }
    Ok(())
}

/// The kernel's capability ABI version this crate speaks (`_LINUX_CAPABILITY_VERSION_3`).
const CAP_VERSION_3: u32 = 0x2008_0522;

/// The number of 32-bit words in an ABI version 3 capability set
/// (`_LINUX_CAPABILITY_U32S_3`).
const CAP_WORDS: usize = 2;

/// `struct __user_cap_header_struct` (`include/uapi/linux/capability.h`).
#[repr(C)]
#[derive(Default)]
struct CapHeader {
    version: u32,
    pid: i32,
}

/// `struct __user_cap_data_struct` (`include/uapi/linux/capability.h`).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Read the process's own capability sets.
fn capability_sets() -> Result<[CapData; CAP_WORDS], SyscallError> {
    let mut header = CapHeader {
        version: CAP_VERSION_3,
        pid: 0, // 0 means "this process", and is what `capset` requires.
    };
    let mut data = [CapData::default(); CAP_WORDS];
    // SAFETY: both pointers are to owned buffers of the sizes the kernel ABI
    // describes, and `pid` is 0, so the kernel reads no other address.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_capget,
            (&raw mut header).cast::<libc::c_void>(),
            data.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if rc == 0 {
        Ok(data)
    } else {
        Err(SyscallError::last_os_error("capget"))
    }
}

/// Write back capability sets read by [`capability_sets`].
fn write_capability_sets(data: &[CapData; CAP_WORDS]) -> Result<(), SyscallError> {
    let mut header = CapHeader {
        version: CAP_VERSION_3,
        pid: 0,
    };
    // SAFETY: `data` is two words as the ABI requires, and the header says so
    // through its version.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_capset,
            (&raw mut header).cast::<libc::c_void>(),
            data.as_ptr().cast::<libc::c_void>(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(SyscallError::last_os_error("capset"))
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

    /// The table is ABI: a wrong number means a service asking for
    /// `NET_BIND_SERVICE` silently gets something else. Spot-checked against
    /// `include/uapi/linux/capability.h`, including the first and last entries.
    #[test]
    fn capability_numbers_are_the_kernels() {
        assert_eq!(capability_number("chown"), Some(0));
        assert_eq!(capability_number("setpcap"), Some(8));
        assert_eq!(capability_number("net_bind_service"), Some(10));
        assert_eq!(capability_number("sys_admin"), Some(21));
        assert_eq!(capability_number("checkpoint_restore"), Some(40));
    }

    /// Every name in the table has a distinct, sane number: a duplicate would
    /// make one of the two capabilities unreachable.
    #[test]
    fn every_name_has_its_own_number() {
        let mut seen: Vec<u32> = CAPABILITY_NAMES.values().copied().collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "two names share a number");
        assert_eq!(seen.len(), 41, "capabilities 0..=40 exist on this kernel");
        assert!(seen.iter().all(|n| *n <= MAX_CAPABILITY));
        assert_eq!(
            seen,
            (0..=40).collect::<Vec<u32>>(),
            "the table must be the whole range, not a sample"
        );
    }

    /// A name the kernel does not have is `None`, which the resolver turns into
    /// a failed unit rather than a smaller privilege drop.
    #[test]
    fn an_unknown_capability_has_no_number() {
        assert_eq!(capability_number("bogus"), None);
        assert_eq!(capability_number("cap_sys_admin"), None, "CAP_ is stripped");
        assert_eq!(capability_number("SYS_ADMIN"), None, "names are lower-case");
    }

    /// Reading the sets needs no privilege - and is what makes the inheritable
    /// step able to leave permitted and effective untouched.
    #[test]
    fn the_capability_sets_can_always_be_read() {
        capability_sets().expect("capget on one's own process");
    }

    /// Dropping a bounding capability needs `CAP_SETPCAP` (plan §7.4: the
    /// reason this step precedes `setuid`). As root it succeeds, so the
    /// assertion is the unprivileged half, and says so.
    #[test]
    fn dropping_the_bounding_set_needs_privilege() {
        if process::geteuid() == 0 {
            return; // root has CAP_SETPCAP; the guest covers that path.
        }
        let err = set_bounding_set(&[]).expect_err("an unprivileged process cannot drop caps");
        assert_eq!(err.errno(), libc::EPERM);
        assert_eq!(err.op(), "prctl(PR_CAPBSET_DROP)");
    }

    /// Growing the inheritable set needs `CAP_SETPCAP` too - and this is the
    /// step that would silently do nothing if it ran after the bounding set had
    /// already been narrowed, so it matters that its failure is loud.
    #[test]
    fn growing_the_inheritable_set_needs_privilege() {
        if process::geteuid() == 0 {
            return; // root has CAP_SETPCAP; the guest covers that path.
        }
        let err = set_inheritable(&[10]).expect_err("an unprivileged process cannot inherit caps");
        assert_eq!(err.errno(), libc::EPERM);
        assert_eq!(err.op(), "capset");
    }

    /// Raising an ambient capability needs the bit in permitted *and*
    /// inheritable, neither of which an unprivileged process has.
    #[test]
    fn raising_an_ambient_capability_needs_privilege() {
        if process::geteuid() == 0 {
            return; // root has both; the guest covers that path.
        }
        let err =
            set_ambient(&[10]).expect_err("an unprivileged process cannot raise ambient caps");
        assert_eq!(err.errno(), libc::EPERM);
        assert_eq!(err.op(), "prctl(PR_CAP_AMBIENT_RAISE)");
    }

    /// Keeping capabilities across a uid change is not itself privileged - the
    /// uid change is what is refused. Doing it here is safe for the rest of the
    /// test binary: nothing in it changes uid from root.
    #[test]
    fn keeping_capabilities_is_allowed_unprivileged() {
        keep_capabilities().expect("PR_SET_KEEPCAPS needs no privilege of its own");
    }

    /// An empty ambient list is a clear, and clearing is always allowed -
    /// including for a process that never had any.
    #[test]
    fn clearing_the_ambient_set_is_always_allowed() {
        set_ambient(&[]).expect("clearing the ambient set needs no privilege");
    }
}
