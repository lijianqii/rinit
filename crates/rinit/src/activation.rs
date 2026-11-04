//! Socket activation (plan §7.9).
//!
//! A unit with `SocketActivate=` is not started at boot: rinit binds the socket
//! and holds it, and the *first connection* is what starts the service. The plan
//! puts the value in the memory an idle system does not spend ("空闲服务不常驻"), and
//! the mechanism has three parts:
//!
//! 1. **rinit owns the listening socket**, for as long as it runs. That is what
//!    makes the service disposable: it can stop and start again without the socket
//!    ever going away, so a client never sees the difference.
//! 2. **The connection is the signal, and the service is the one that accepts.**
//!    The reactor watches the socket for readability and tells the main loop;
//!    nothing accepts here, because the accepted connection belongs to the service
//!    this is starting. What rinit hands over is the *listening* socket, on fd 3
//!    with `LISTEN_FDS=1` in the environment - the contract `sd_listen_fds`
//!    implements.
//! 3. **`IdleTimeoutSec=` stops it again.** "Idle" is counted from the last
//!    *arrival* - the unit's start, or a connection since - and a service serving
//!    one long-lived connection therefore looks idle, exactly as it does under
//!    systemd. That is why the directive defaults to 0, never stop, and why a
//!    request/response service is the one to use it on.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::Duration;

use rinit_core::UnitName;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

use crate::log::rlog;

/// The environment socket-activated services read to find their socket
/// (plan §7.9).
///
/// One descriptor, on fd 3: the plan asks for one listening socket per unit, so
/// there is no range and no per-socket name to carry.
pub const LISTEN_FDS_ENV: &str = "LISTEN_FDS=1";

/// The sockets rinit holds for units that asked for one.
pub struct Activated {
    /// One per unit, keyed by name.
    sockets: BTreeMap<UnitName, Bound>,
    /// Where the watchers report a connection.
    ready: mpsc::UnboundedReceiver<UnitName>,
    /// Held so `ready` never resolves to `None` and the loop's branch stays
    /// pending instead of spinning.
    sender: mpsc::UnboundedSender<UnitName>,
}

/// One listening socket, its path, and the task watching it.
struct Bound {
    /// The listening socket. rinit owns it; a service gets a `dup2` of it, so it
    /// can come and go without the socket doing the same.
    listener: std::os::unix::net::UnixListener,
    path: PathBuf,
    /// One permit, one wake-up. The watcher consumes it when it reports a
    /// connection and does not ask again until the main loop rearms it.
    permit: mpsc::Sender<()>,
    _watch: tokio::task::JoinHandle<()>,
}

impl Activated {
    /// No sockets: every unit starts the ordinary way.
    pub fn disabled() -> Self {
        let (sender, ready) = mpsc::unbounded_channel();
        Activated {
            sockets: BTreeMap::new(),
            ready,
            sender,
        }
    }

    /// Bind `path` for `unit`, replacing any socket left at that path.
    ///
    /// The mode is set explicitly: `bind` leaves the socket at `0666 & ~umask`,
    /// and a socket nobody may connect to is a service nobody can start. `0666` is
    /// systemd's own default for `SocketMode=`, and the directory the socket lives
    /// in is where a deployment restricts access.
    ///
    /// # Errors
    /// A directory that cannot be created, a socket that cannot be bound, or one
    /// whose mode cannot be set - all of which mean the unit has no socket to be
    /// activated on, which its start has to know about.
    pub fn bind(&mut self, unit: &UnitName, path: &Path) -> io::Result<()> {
        // Re-binding the same path is a no-op. A reload must not unlink and
        // recreate a live listener just because the definition was re-read.
        if self
            .sockets
            .get(unit)
            .is_some_and(|bound| bound.path.as_path() == path)
        {
            return Ok(());
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        // A socket left behind by a previous rinit would make `bind` fail with
        // `EADDRINUSE`; nothing else is supposed to be at this path.
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;

        // The watcher gets its own descriptor, so handing the original to a child
        // does not disturb it - and so `register` has an `OwnedFd` that outlives
        // the wait, which is the obligation it cannot check for itself.
        let watched = OwnedFd::from(listener.try_clone()?);
        let sender = self.sender.clone();
        let watched_unit = unit.clone();
        let (permit, permit_rx) = mpsc::channel(1);
        // The first connection is allowed through without the main loop having
        // to ask: a socket that is bound but never reported would be dead.
        let _ = permit.try_send(());
        let watch =
            tokio::spawn(async move { watch(watched, sender, permit_rx, watched_unit).await });

        rlog!("{unit}: listening on {}", path.display());
        let new = Bound {
            listener,
            path: path.to_path_buf(),
            permit,
            _watch: watch,
        };
        if let Some(old) = self.sockets.insert(unit.clone(), new) {
            // The new listener is already bound. Only now is it safe to take the
            // old one down; if the bind above had failed, the old path and its
            // watcher would still be intact.
            old._watch.abort();
            if old.path.as_path() != path {
                let _ = std::fs::remove_file(&old.path);
            }
        }
        Ok(())
    }

    /// The descriptor to hand `unit`'s service, if rinit is holding one.
    pub fn listener_fd(&self, unit: &UnitName) -> Option<std::os::fd::RawFd> {
        self.sockets
            .get(unit)
            .map(|bound| bound.listener.as_raw_fd())
    }

    /// The path currently bound for `unit`, if any.
    pub fn path_of(&self, unit: &UnitName) -> Option<&Path> {
        self.sockets.get(unit).map(|bound| bound.path.as_path())
    }

    /// Drop `unit`'s socket and watcher, for a reload that removed or changed
    /// `SocketActivate=`.
    pub fn remove(&mut self, unit: &UnitName) {
        if let Some(bound) = self.sockets.remove(unit) {
            bound._watch.abort();
            let _ = std::fs::remove_file(&bound.path);
        }
    }

    /// The units whose sockets rinit currently holds.
    pub fn units(&self) -> Vec<UnitName> {
        self.sockets.keys().cloned().collect()
    }

    /// Allow the watcher for `unit` to report one more connection.
    ///
    /// At most one permit is queued, so calling this while the unit is idle is
    /// idempotent and does not wake the loop by itself.
    pub fn rearm(&mut self, unit: &UnitName) {
        if let Some(bound) = self.sockets.get(unit) {
            let _ = bound.permit.try_send(());
        }
    }

    /// A unit whose socket has a connection waiting, if any.
    pub async fn recv(&mut self) -> Option<UnitName> {
        self.ready.recv().await
    }

    /// Remove the socket files on shutdown, so a later run can bind them again.
    pub fn disarm(&self) {
        for bound in self.sockets.values() {
            let _ = std::fs::remove_file(&bound.path);
        }
    }
}

/// Report every connection `listener` gets, for as long as the task lives.
///
/// Readability, not `accept`: the connection is the service's, and taking it here
/// would mean rinit had to pass the accepted stream instead of the socket the
/// service is written to expect.
async fn watch(
    listener: OwnedFd,
    sender: mpsc::UnboundedSender<UnitName>,
    mut permit: mpsc::Receiver<()>,
    unit: UnitName,
) {
    // SAFETY: `listener` is an `OwnedFd`, so this call owns a valid descriptor and
    // never closes or replaces it while the `AsyncFd` lives - the obligation
    // `register` documents. `AsyncFd::new` is deprecated as unsound for exactly
    // that reason.
    let afd = match unsafe { AsyncFd::register(listener) } {
        Ok(afd) => afd,
        Err(e) => {
            rlog!("warning: cannot watch {unit}'s socket: {e}");
            return;
        }
    };
    loop {
        // One permit, one report. A level-triggered listener stays readable
        // until the service accepts, so without this wait the watcher would
        // report the same queued connection as fast as the reactor can run it.
        if permit.recv().await.is_none() {
            return;
        }
        let mut guard = match afd.readable().await {
            Ok(guard) => guard,
            Err(e) => {
                rlog!("warning: {unit}'s socket became unwatchable: {e}");
                return;
            }
        };
        // A send failure means the main loop is gone, which is the process
        // shutting down: nothing left to wake.
        if sender.send(unit.clone()).is_err() {
            return;
        }
        guard.clear_ready();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn name(s: &str) -> UnitName {
        UnitName::parse(s).expect("valid unit name")
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rinit-activation-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir
    }

    /// Binding is what makes a unit socket-activated at all: the socket exists,
    /// anyone may connect, rinit can hand its descriptor over, and shutdown
    /// removes the file so the next run can bind again.
    #[tokio::test(flavor = "current_thread")]
    async fn bind_creates_the_socket_and_disarm_removes_it() {
        let dir = scratch("bind");
        let path = dir.join("nested").join("svc.sock");
        let mut activated = Activated::disabled();
        let unit = name("svc.service");

        activated.bind(&unit, &path).expect("bind");

        assert!(path.exists(), "bind creates missing parents and the socket");
        let mode = std::fs::metadata(&path)
            .expect("stat the socket")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o666, "the socket must be connectable");
        assert!(activated.listener_fd(&unit).is_some());
        assert!(
            activated.listener_fd(&name("other.service")).is_none(),
            "a unit without a socket has no descriptor"
        );

        activated.disarm();
        assert!(!path.exists(), "disarm removes the socket file");

        drop(activated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reload changes the path by binding the new socket first. A failed
    /// bind must leave the old listener and file in place.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_rebind_keeps_the_old_socket() {
        let dir = scratch("rebind-fail");
        let old = dir.join("old.sock");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker");
        let bad = blocker.join("new.sock");
        let unit = name("svc.service");
        let mut activated = Activated::disabled();

        activated.bind(&unit, &old).expect("old bind");
        assert!(activated.bind(&unit, &bad).is_err());
        assert!(old.exists(), "the old path must survive a failed rebind");
        assert_eq!(activated.path_of(&unit), Some(old.as_path()));
        assert!(activated.listener_fd(&unit).is_some());

        activated.disarm();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A successful path change binds the replacement before removing the old
    /// file.
    #[tokio::test(flavor = "current_thread")]
    async fn a_rebind_replaces_the_path_atomically() {
        let dir = scratch("rebind");
        let old = dir.join("old.sock");
        let new = dir.join("new.sock");
        let unit = name("svc.service");
        let mut activated = Activated::disabled();

        activated.bind(&unit, &old).expect("old bind");
        activated.bind(&unit, &new).expect("new bind");
        assert!(!old.exists(), "the old path is released after the new bind");
        assert!(new.exists());
        assert_eq!(activated.path_of(&unit), Some(new.as_path()));

        activated.disarm();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A socket left at the path by a previous rinit must not make the next
    /// bind fail with `EADDRINUSE`.
    #[tokio::test(flavor = "current_thread")]
    async fn bind_replaces_a_file_left_at_the_path() {
        let dir = scratch("stale");
        let path = dir.join("svc.sock");
        std::fs::write(&path, b"not a socket").expect("stale file");
        let mut activated = Activated::disabled();

        activated
            .bind(&name("svc.service"), &path)
            .expect("bind replaces the stale entry");
        assert!(activated.listener_fd(&name("svc.service")).is_some());

        activated.disarm();
        drop(activated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The watcher's whole job: a connection becomes the unit's name on the
    /// channel the main loop selects on.
    #[tokio::test(flavor = "current_thread")]
    async fn a_connection_reports_the_unit() {
        let dir = scratch("wake");
        let path = dir.join("svc.sock");
        let mut activated = Activated::disabled();
        let unit = name("svc.service");
        activated.bind(&unit, &path).expect("bind");

        let client = UnixStream::connect(&path).expect("connect");
        let woken = tokio::time::timeout(Duration::from_secs(2), activated.recv())
            .await
            .expect("a queued connection must wake the loop")
            .expect("rinit still holds the sender");
        assert_eq!(woken, unit);

        // The watcher does not keep reporting the same queued connection.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), activated.recv())
                .await
                .is_err(),
            "a second report must wait for an explicit rearm"
        );
        activated.rearm(&unit);
        let second = UnixStream::connect(&path).expect("second connect");
        let woken = tokio::time::timeout(Duration::from_secs(2), activated.recv())
            .await
            .expect("rearm must let the watcher report a new connection")
            .expect("rinit still holds the sender");
        assert_eq!(woken, unit);

        drop(second);
        drop(client);
        activated.disarm();
        drop(activated);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
