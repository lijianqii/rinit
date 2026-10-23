//! The `/run/rinit/control.sock` server (plan §1.3(10), §3 M3).
//!
//! Transport only: bind, authenticate, frame, hand the request to the main
//! loop, write the reply. Everything about *what* a request means lives in
//! `boot::handle_control`, because that is where the manager and the notify hub
//! are.
//!
//! The security rules are the plan's, and they are all enforced here before a
//! single command byte is read:
//!
//! - the directory is `0700` and the socket `0600` (plan §1.3(10));
//! - `bind` runs under `umask(0o177)` so the socket is never briefly wider than
//!   `0600`, which would otherwise be a real race even with the directory
//!   restricting access;
//! - `SO_PEERCRED` decides who the client is, and anything but root is refused.
//!   The client cannot lie about this: the kernel fills it in at connect time.
//!   `--dev` narrows this to "the user rinit runs as" (see [`peer_allowed`]),
//!   which is what makes `RINIT_CONTROL_DIR` usable without root.

use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::time::Duration;

use rinit_core::{
    LENGTH_PREFIX_BYTES, ProtocolError, Request, Status, decode_request, encode_response, frame,
    frame_len,
};
use rinit_runtime::RuntimeError;
use rinit_runtime::syscall::fs;
use rinit_runtime::syscall::process;
use rinit_runtime::syscall::socket::peer_credentials;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::log::rlog;

/// The directory holding the control socket (plan §1.3(10)).
///
/// Re-exported from `rinit-core` so the server, the client and the tests all
/// agree on one name.
pub use rinit_core::{CONTROL_DIR, ENV_CONTROL_DIR};

/// How long a connection may take to send its request.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the main loop may take to answer a request.
///
/// `TimeoutStartSec` defaults to 30 s and `TimeoutStopSec` to 10 s, so a
/// `start`, `stop` or `restart` can legitimately wait for a whole job. 60 s
/// covers both with room to spare; past that the manager is not going to
/// answer this request in any useful sense, and holding the connection open
/// leaves the client blocked in `read` forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// A request from a client, with the channel to answer on.
pub type Incoming = (Request, oneshot::Sender<(Status, String)>);

/// The bound control socket and the queue of requests it has read.
pub struct ControlServer {
    receiver: mpsc::UnboundedReceiver<Incoming>,
    /// Held so `recv` stays *pending* rather than resolving to `None` when
    /// there is no listener: a branch that is always ready would make the main
    /// loop spin.
    _sender: mpsc::UnboundedSender<Incoming>,
    /// The socket path to remove on shutdown, when one was bound.
    socket: Option<PathBuf>,
    _accept: Option<JoinHandle<()>>,
}

impl ControlServer {
    /// Bind the control socket, or return a disabled server if that fails.
    ///
    /// Binding is best effort on purpose: a system where `/run` is not writable
    /// must still boot, it just cannot be controlled live.
    /// `dev` relaxes the peer rule to "the user rinit runs as" (see
    /// [`peer_allowed`]); production calls this with `false`.
    pub fn bind(dev: bool) -> Self {
        match Self::try_bind(dev) {
            Ok(server) => server,
            Err(e) => {
                rlog!("warning: control socket unavailable: {e}");
                Self::disabled()
            }
        }
    }

    /// A server that never yields a request.
    pub fn disabled() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        ControlServer {
            receiver,
            _sender: sender,
            socket: None,
            _accept: None,
        }
    }

    fn try_bind(dev: bool) -> Result<Self, RuntimeError> {
        let dir = control_dir();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| RuntimeError::Io {
                op: "create control dir",
                source: e,
            })?;
        let path = dir.join("control.sock");
        // A stale socket from a previous run would make `bind` fail with
        // EADDRINUSE; nothing else is supposed to be at this path.
        let _ = std::fs::remove_file(&path);

        // Close the permission window: `bind` creates the socket through the
        // process umask, so rinit sets it for exactly that call (plan §1.3(10))
        // and restores it afterwards. Keep the owner's execute bit in the mask:
        // the test suite runs these tests in parallel, and a directory created
        // by another thread inside this window has to stay traversable. The
        // socket is 0600 either way, and the explicit chmod below is the belt.
        let previous = fs::umask(0o077);
        let listener = UnixListener::bind(&path);
        fs::umask(previous);
        let listener = listener.map_err(|e| RuntimeError::Io {
            op: "bind control socket",
            source: e,
        })?;
        // Belt and braces: the umask already produced 0600, and this also
        // covers a socket left behind with a wider mode. A non-UTF-8 path is a
        // caller error, named as such rather than reported as ENOENT on "".
        let path_str = path.to_str().ok_or_else(|| RuntimeError::Io {
            op: "control socket path",
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "control socket path is not valid UTF-8",
            ),
        })?;
        fs::chmod(path_str, 0o600)?;

        let (sender, receiver) = mpsc::unbounded_channel();
        let task_sender = sender.clone();
        let accept = tokio::spawn(accept_loop(listener, task_sender, dev));

        Ok(ControlServer {
            receiver,
            _sender: sender,
            socket: Some(path),
            _accept: Some(accept),
        })
    }

    /// Await the next request from a client.
    pub async fn recv(&mut self) -> Option<Incoming> {
        self.receiver.recv().await
    }

    /// Remove the socket, so a later run can bind it again.
    pub fn disarm(&self) {
        if let Some(path) = &self.socket {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn control_dir() -> PathBuf {
    match std::env::var(ENV_CONTROL_DIR) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(CONTROL_DIR),
    }
}

/// Accept connections forever, one task each.
async fn accept_loop(listener: UnixListener, sender: mpsc::UnboundedSender<Incoming>, dev: bool) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let sender = sender.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(stream, sender, dev).await {
                        // A client hanging up mid-request is normal, not worth
                        // more than a line.
                        rlog!("control: connection ended: {e}");
                    }
                });
            }
            Err(e) => {
                // Out of descriptors, or the listener was closed. Back off so a
                // persistent error cannot spin the reactor.
                rlog!("warning: control accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Whether a peer with `peer_uid` may use the control socket.
///
/// Root only (plan §1.3(10)) - with one exception: in `--dev` rinit is not PID 1
/// and the socket lives wherever `RINIT_CONTROL_DIR` says, so the rule becomes
/// "only the user rinit itself runs as". Without that, `RINIT_CONTROL_DIR` buys
/// a non-root developer a socket that refuses every command, and the whole
/// dispatch path becomes untestable outside the guest. The production rule is
/// untouched, and the property that matters - only the owner may talk - holds
/// either way.
fn peer_allowed(dev: bool, peer_uid: u32, our_uid: u32) -> bool {
    peer_uid == 0 || (dev && peer_uid == our_uid)
}

/// Authenticate, read one framed request, dispatch it, write one framed reply.
async fn serve(
    mut stream: UnixStream,
    sender: mpsc::UnboundedSender<Incoming>,
    dev: bool,
) -> std::io::Result<()> {
    // Authentication first: no command byte is read until the peer is known to
    // be allowed (plan §1.3(10)).
    let cred = match peer_credentials(stream.as_raw_fd()) {
        Ok(cred) => cred,
        Err(e) => {
            return write_response(
                &mut stream,
                Status::Error,
                &format!("cannot read peer credentials: {e}"),
            )
            .await;
        }
    };
    if !peer_allowed(dev, cred.uid, process::geteuid()) {
        rlog!(
            "warning: refusing control connection from uid {} (pid {})",
            cred.uid,
            cred.pid
        );
        return write_response(
            &mut stream,
            Status::Error,
            "the control socket is restricted to root",
        )
        .await;
    }

    let body = match read_frame(&mut stream).await {
        Ok(body) => body,
        Err(FrameError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            // The client connected and went away; nothing to answer.
            return Ok(());
        }
        Err(FrameError::Io(e)) => return Err(e),
        Err(FrameError::Protocol(e)) => {
            return write_response(&mut stream, status_of(&e), &e.to_string()).await;
        }
    };

    let request = match decode_request(&body) {
        Ok(request) => request,
        Err(e) => {
            return write_response(&mut stream, status_of(&e), &e.to_string()).await;
        }
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    if sender.send((request, reply_tx)).is_err() {
        return write_response(&mut stream, Status::Error, "rinit is shutting down").await;
    }
    let (status, text) = await_reply(reply_rx, REPLY_TIMEOUT).await;
    write_response(&mut stream, status, &text).await
}

/// Wait for the main loop to answer, but not forever.
///
/// A request the manager accepted and then never completed would otherwise
/// pin this connection open for as long as the client is willing to wait,
/// which for a plain `read` is forever. Timing out here is a reply: the
/// client still gets a status and a sentence, instead of a hang it cannot
/// distinguish from rinit being busy - or wedged.
async fn await_reply(
    reply_rx: oneshot::Receiver<(Status, String)>,
    timeout: Duration,
) -> (Status, String) {
    match tokio::time::timeout(timeout, reply_rx).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(_)) => (Status::Error, "the request was dropped".to_string()),
        Err(_) => (
            Status::Error,
            "timed out waiting for the manager; the request may still be running".to_string(),
        ),
    }
}

/// How a bad frame is reported to the client.
fn status_of(error: &ProtocolError) -> Status {
    match error {
        ProtocolError::VersionMismatch { .. } => Status::VersionMismatch,
        ProtocolError::UnknownCode { .. } => Status::UnknownCommand,
        _ => Status::BadRequest,
    }
}

/// What went wrong reading a frame.
enum FrameError {
    /// The socket itself failed (including a clean EOF).
    Io(std::io::Error),
    /// The frame was malformed; the client gets a reply.
    Protocol(ProtocolError),
}

/// Read one length-prefixed frame, bounded by [`IO_TIMEOUT`].
async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>, FrameError> {
    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut prefix))
        .await
        .map_err(|_| {
            FrameError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for a request",
            ))
        })?
        .map_err(FrameError::Io)?;

    let len = frame_len(prefix).map_err(FrameError::Protocol)?;
    let mut body = vec![0u8; len];
    tokio::time::timeout(IO_TIMEOUT, stream.read_exact(&mut body))
        .await
        .map_err(|_| {
            FrameError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out reading the request body",
            ))
        })?
        .map_err(FrameError::Io)?;
    Ok(body)
}

/// Write one length-prefixed response and close the write half.
async fn write_response(
    stream: &mut UnixStream,
    status: Status,
    text: &str,
) -> std::io::Result<()> {
    let body = encode_response(status, text);
    stream.write_all(&frame(&body)).await?;
    stream.flush().await?;
    // The client reads to EOF; half-close so it does not wait for us.
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production rule: root, and only root.
    #[test]
    fn only_root_may_use_the_socket_in_production() {
        assert!(peer_allowed(false, 0, 0));
        assert!(peer_allowed(false, 0, 1000));
        assert!(!peer_allowed(false, 1000, 0));
        assert!(
            !peer_allowed(false, 1000, 1000),
            "--dev is what relaxes this; without it, rinit's own user is refused too"
        );
    }

    /// `--dev` narrows the rule to "the user rinit runs as", so a socket under
    /// `RINIT_CONTROL_DIR` can actually be used by the developer who started it.
    #[test]
    fn dev_mode_admits_the_invoking_user_only() {
        assert!(peer_allowed(true, 1000, 1000), "the owner may talk");
        assert!(
            !peer_allowed(true, 1000, 4242),
            "another user still may not"
        );
        assert!(!peer_allowed(true, 4242, 1000));
    }

    /// Root is admitted by the first clause in both modes, so a root `rctl`
    /// keeps working against a `--dev` rinit.
    #[test]
    fn root_is_admitted_even_in_dev_mode() {
        assert!(peer_allowed(true, 0, 1000));
    }

    /// A job the manager never finishes still has to produce a reply; the
    /// production timeout is 60 s, so the test gives the same code 10 ms.
    #[tokio::test(flavor = "current_thread")]
    async fn a_request_that_is_never_answered_times_out() {
        let (_tx, rx) = oneshot::channel();
        let (status, text) = await_reply(rx, Duration::from_millis(10)).await;
        assert_eq!(status, Status::Error);
        assert!(text.contains("timed out"), "{text}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_request_is_reported_as_dropped() {
        let (tx, rx) = oneshot::channel();
        drop(tx);
        let (status, text) = await_reply(rx, Duration::from_secs(1)).await;
        assert_eq!(status, Status::Error);
        assert!(text.contains("dropped"), "{text}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_answer_within_the_timeout_is_passed_through() {
        let (tx, rx) = oneshot::channel();
        tx.send((Status::Ok, "fine".to_string()))
            .expect("the receiver is alive");
        let (status, text) = await_reply(rx, Duration::from_secs(1)).await;
        assert_eq!(status, Status::Ok);
        assert_eq!(text, "fine");
    }
}
