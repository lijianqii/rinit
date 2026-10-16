//! Socket helpers for the control plane (plan §1.3(10)).
//!
//! Binding and connecting are done through `tokio::net::UnixListener` /
//! `UnixStream`; what those do not expose is the *peer's credentials*, which is
//! the whole authentication story for the control socket: the kernel told us at
//! connect time who the client is, so rinit never has to trust anything the
//! client says about itself.
//!
//! Keeping the `getsockopt` here means the only `unsafe` stays in the audited
//! syscall layer (plan §2.3).

use crate::error::SyscallError;

/// Who the kernel says is on the other end of a connected Unix socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    /// The peer's process id.
    pub pid: libc::pid_t,
    /// The peer's effective uid.
    pub uid: libc::uid_t,
    /// The peer's effective gid.
    pub gid: libc::gid_t,
}

impl PeerCred {
    /// Whether the peer is root, the only uid allowed to use the control
    /// socket (plan §1.3(10)).
    pub const fn is_root(self) -> bool {
        self.uid == 0
    }
}

/// Read `SO_PEERCRED` from a connected Unix socket.
///
/// # Errors
/// [`SyscallError`] from `getsockopt` (e.g. the fd is not a connected Unix
/// socket).
pub fn peer_credentials(fd: libc::c_int) -> Result<PeerCred, SyscallError> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a valid out-buffer of the size reported in `len`, and
    // the option is a fixed-size struct for this socket family.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(SyscallError::last_os_error("getsockopt(SO_PEERCRED)"));
    }
    Ok(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
        gid: cred.gid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel reports *our* credentials for a socketpair's other end, which
    /// is what makes the control socket's authentication trustworthy.
    #[test]
    fn peer_credentials_describe_a_real_socketpair() {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a valid 2-element out-buffer.
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );

        let cred = peer_credentials(fds[0]).expect("SO_PEERCRED on a unix socket");
        // SAFETY: the two descriptors are open; this test reads its own ids.
        let (uid, gid, pid) = unsafe { (libc::geteuid(), libc::getegid(), libc::getpid()) };
        assert_eq!(cred.uid, uid);
        assert_eq!(cred.gid, gid);
        assert_eq!(cred.pid, pid);
        assert_eq!(cred.is_root(), uid == 0);

        // SAFETY: both descriptors came from `socketpair` and are not owned by
        // anything else.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }

    /// A non-socket fd reports the error instead of a bogus credential.
    #[test]
    fn peer_credentials_reject_a_non_socket() {
        // SAFETY: `-1` is never a valid descriptor, so nothing is affected.
        assert!(peer_credentials(-1).is_err());
    }
}
