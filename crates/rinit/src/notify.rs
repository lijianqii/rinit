//! Per-unit `notify` sockets (plan §1.3(12)).
//!
//! A `Type=notify` service is only ready once it says so. rinit gives each such
//! unit its own Unix datagram socket, tells the service where it is through
//! `NOTIFY_SOCKET`, and reads `READY=1` / `STATUS=` lines from it.
//!
//! The socket is owned by the unit's `User=`/`Group=` and mode `0660`, so one
//! service cannot forge another's readiness. rinit reads but never writes these
//! sockets; the sender is a never-dropped channel kept in the hub, so
//! [`NotifyHub::recv`] simply waits when nothing is armed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rinit_core::{NotifyMessage, UnitConfig, UnitName, parse_notify};
use rinit_runtime::RuntimeError;
use rinit_runtime::syscall::fs;
use tokio::net::UnixDatagram;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::log::rlog;

/// Where the per-unit notify sockets are created (plan §1.3(12)).
pub const NOTIFY_DIR: &str = "/run/rinit/notify";

/// The directory mode: readable/traversable, but only root may create entries.
const DIR_MODE: u32 = 0o755;
/// The socket mode: writable only by its owner (the unit's user, or root).
const SOCKET_MODE: u32 = 0o660;
/// The `umask` used around `bind`, chosen so `0777 & ~umask == 0660`.
const BIND_UMASK: u32 = 0o117;
/// The largest datagram rinit will read. sd_notify messages are tiny; anything
/// larger is truncated by the kernel anyway.
const MAX_DATAGRAM: usize = 4096;

/// One armed unit: its socket path and the task reading it.
struct Armed {
    path: PathBuf,
    reader: JoinHandle<()>,
}

/// Owns every notify socket and funnels their messages into one stream.
pub struct NotifyHub {
    dir: PathBuf,
    sender: mpsc::UnboundedSender<(UnitName, NotifyMessage)>,
    receiver: mpsc::UnboundedReceiver<(UnitName, NotifyMessage)>,
    armed: BTreeMap<UnitName, Armed>,
}

impl NotifyHub {
    /// A hub using [`NOTIFY_DIR`].
    pub fn new() -> Self {
        Self::new_at(NOTIFY_DIR)
    }

    /// A hub using a specific directory (tests use a temporary one).
    pub fn new_at<P: Into<PathBuf>>(dir: P) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        NotifyHub {
            dir: dir.into(),
            sender,
            receiver,
            armed: BTreeMap::new(),
        }
    }

    /// The path of `unit`'s socket, whether or not it is armed.
    pub fn socket_path(&self, unit: &UnitName) -> PathBuf {
        self.dir.join(format!("{}.sock", unit.canonical()))
    }

    /// Create `unit`'s socket and start reading it.
    ///
    /// Arming an already-armed unit replaces the old socket, which is what a
    /// restart needs. Use [`env_for`](Self::env_for) to get the child's
    /// `NOTIFY_SOCKET` value.
    ///
    /// # Errors
    /// Propagates a directory, bind, `chown` or `chmod` failure.
    pub fn arm(&mut self, config: &UnitConfig) -> Result<(), RuntimeError> {
        let unit = config.name.clone();
        let path = self.socket_path(&unit);
        ensure_dir(&self.dir)?;

        // A socket left over from a previous run would make `bind` fail with
        // `EADDRINUSE`. The address belongs to this unit, so reclaim it.
        remove_if_present(&path, "notify socket")?;

        let socket = bind_private(&path)?;

        // Plan §1.3(12): only the service's own user may write. root can always
        // write, so an unset `User=`/`Group=` stays root and still works.
        let uid = fs::resolve_uid(&config.user)?;
        let gid = fs::resolve_gid(&config.group)?;
        let path_str = path
            .to_str()
            .ok_or_else(|| RuntimeError::Io {
                op: "notify socket path",
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "socket path is not valid UTF-8",
                ),
            })?
            .to_string();
        // rinit is PID 1 and always root in production, where `chown` always
        // works. On a development host it may be unprivileged, and cannot hand
        // the socket to another user, so the step is skipped there.
        if fs::is_root() {
            fs::chown(&path_str, uid, gid)?;
        }
        fs::chmod(&path_str, SOCKET_MODE)?;

        let reader = self.spawn_reader(unit.clone(), socket);
        if let Some(previous) = self.armed.insert(unit, Armed { path, reader }) {
            // A previous socket for the same unit is stale by definition.
            previous.reader.abort();
        }
        Ok(())
    }

    /// The `NOTIFY_SOCKET=...` variable for `unit`, if it is armed.
    pub fn env_for(&self, unit: &UnitName) -> Option<String> {
        self.armed
            .get(unit)
            .map(|armed| format!("NOTIFY_SOCKET={}", armed.path.display()))
    }

    /// Wait for the next notify message from any armed unit.
    ///
    /// Never returns `None` while the hub is alive: the hub holds a sender, so
    /// the channel cannot close. It is safe to `select!` on.
    pub async fn recv(&mut self) -> Option<(UnitName, NotifyMessage)> {
        self.receiver.recv().await
    }

    /// Remove `unit`'s socket and stop reading it.
    pub fn disarm(&mut self, unit: &UnitName) {
        if let Some(armed) = self.armed.remove(unit) {
            armed.reader.abort();
            let _ = std::fs::remove_file(&armed.path);
        }
    }

    /// Remove every socket; used at shutdown.
    pub fn disarm_all(&mut self) {
        let units: Vec<UnitName> = self.armed.keys().cloned().collect();
        for unit in units {
            self.disarm(&unit);
        }
    }

    fn spawn_reader(&self, unit: UnitName, socket: UnixDatagram) -> JoinHandle<()> {
        let sender = self.sender.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            // A recv error on a datagram socket means the socket is unusable
            // (e.g. it was closed), so stop rather than spin.
            while let Ok(n) = socket.recv(&mut buf).await {
                match parse_notify(&buf[..n]) {
                    Ok(message) => {
                        if sender.send((unit.clone(), message)).is_err() {
                            // The hub is gone; nothing left to notify.
                            break;
                        }
                    }
                    Err(e) => rlog!("warning: {unit} sent a bad notify message: {e}"),
                }
            }
        })
    }
}

impl Default for NotifyHub {
    fn default() -> Self {
        Self::new()
    }
}

/// Create `dir` if needed and force its mode to `0755`.
fn ensure_dir(dir: &Path) -> Result<(), RuntimeError> {
    std::fs::create_dir_all(dir).map_err(|e| RuntimeError::Io {
        op: "create notify dir",
        source: e,
    })?;
    let dir_str = dir.to_str().ok_or_else(|| RuntimeError::Io {
        op: "notify dir",
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "notify dir is not valid UTF-8",
        ),
    })?;
    // `create_dir_all` applies the umask; set the intended mode explicitly.
    fs::chmod(dir_str, DIR_MODE)?;
    Ok(())
}

/// Remove `path`, treating "already gone" as success.
fn remove_if_present(path: &Path, what: &'static str) -> Result<(), RuntimeError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(RuntimeError::Io {
            op: what,
            source: e,
        }),
    }
}

/// Bind a datagram socket at `path`, never exposing it more permissively than
/// [`SOCKET_MODE`].
///
/// `bind(2)` creates the socket node with `0777 & ~umask`, so the mode is set
/// *through the umask* here rather than by a later `chmod`: that closes the
/// window in which another process could connect before the `chmod` ran
/// (plan §1.3(12)).
fn bind_private(path: &Path) -> Result<UnixDatagram, RuntimeError> {
    let previous = fs::umask(BIND_UMASK);
    let bound = UnixDatagram::bind(path);
    fs::umask(previous);
    bound.map_err(|e| RuntimeError::Io {
        op: "bind notify socket",
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use rinit_core::ServiceKind;

    fn notify_config(name: &str) -> UnitConfig {
        let mut cfg = UnitConfig::new(UnitName::parse(name).unwrap());
        cfg.service_kind = ServiceKind::Notify;
        cfg
    }

    /// A unique, writable directory per test, so parallel tests do not collide.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rinit-notify-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn an_armed_unit_gets_its_message_through() {
        let dir = temp_dir("roundtrip");
        let mut hub = NotifyHub::new_at(&dir);
        let cfg = notify_config("svc.service");
        hub.arm(&cfg).expect("arm");
        let path = hub.socket_path(&cfg.name);
        assert_eq!(
            hub.env_for(&cfg.name).unwrap(),
            format!("NOTIFY_SOCKET={}", path.display())
        );

        // A client (the service side) sends readiness to the socket.
        let client = std::os::unix::net::UnixDatagram::unbound().unwrap();
        client.send_to(b"READY=1\nSTATUS=up", &path).unwrap();

        let (unit, message) = tokio::time::timeout(std::time::Duration::from_secs(5), hub.recv())
            .await
            .expect("a message must arrive")
            .expect("hub is live");
        assert_eq!(unit, cfg.name);
        assert!(message.ready);
        assert_eq!(message.status.as_deref(), Some("up"));

        hub.disarm_all();
        drop(client);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The socket must not be world-writable, or any service could fake another
    /// unit's readiness (plan §1.3(12)).
    #[tokio::test]
    async fn the_socket_is_owned_and_mode_0660() {
        let dir = temp_dir("mode");
        let mut hub = NotifyHub::new_at(&dir);
        let cfg = notify_config("svc.service");
        hub.arm(&cfg).expect("arm");
        let path = hub.socket_path(&cfg.name);

        let metadata = std::fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o660, "got {mode:o}");
        assert_eq!(metadata.permissions().mode() & 0o007, 0, "no world access");

        hub.disarm_all();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn disarm_removes_the_socket() {
        let dir = temp_dir("disarm");
        let mut hub = NotifyHub::new_at(&dir);
        let cfg = notify_config("svc.service");
        hub.arm(&cfg).expect("arm");
        let path = hub.socket_path(&cfg.name);
        assert!(path.exists());

        hub.disarm(&cfg.name);
        assert!(!path.exists(), "the socket must be unlinked");
        assert!(hub.env_for(&cfg.name).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Re-arming (a restart) must reuse the path rather than fail with
    /// `EADDRINUSE`.
    #[tokio::test]
    async fn rearming_reclaims_a_stale_socket() {
        let dir = temp_dir("rearm");
        let mut hub = NotifyHub::new_at(&dir);
        let cfg = notify_config("svc.service");
        hub.arm(&cfg).expect("first arm");
        hub.arm(&cfg).expect("re-arm must reclaim the path");

        hub.disarm_all();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unset `User=`/`Group=` must default to root, so the common case binds
    /// without needing a lookup that could fail.
    #[tokio::test]
    async fn a_default_owner_arms_successfully() {
        let dir = temp_dir("default-owner");
        let mut hub = NotifyHub::new_at(&dir);
        let cfg = notify_config("svc.service");
        assert!(cfg.user.is_empty() && cfg.group.is_empty());
        hub.arm(&cfg).expect("root-owned socket");

        hub.disarm_all();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unknown `User=` is an error, not a silent fallback to root.
    #[tokio::test]
    async fn an_unknown_user_fails_to_arm() {
        let dir = temp_dir("bad-user");
        let mut hub = NotifyHub::new_at(&dir);
        let mut cfg = notify_config("svc.service");
        cfg.user = "rinit-no-such-user-xyz".to_string();
        let err = hub.arm(&cfg).expect_err("an unknown user must fail");
        assert!(err.to_string().contains("getpwnam"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
