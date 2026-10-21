//! The socket half of `rctl`: connect, send one request, read one reply.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use rinit_core::{
    ENV_CONTROL_DIR, LENGTH_PREFIX_BYTES, Request, Status, decode_response, encode_request, frame,
    frame_len,
};

/// The longest `rctl` waits for a reply.
///
/// The server gives the manager 60 s and then sends its own timeout reply; 65 s
/// leaves 5 s for that reply to arrive, so the client normally reports the
/// server's sentence ("timed out waiting for the manager") instead of a bare
/// socket timeout. Without it, a server that stops sending would leave `rctl`
/// blocked in `read` forever, and a CI job hanging looks like the machine, not
/// like the bug.
const REPLY_TIMEOUT: Duration = Duration::from_secs(65);

/// Send `request` to rinit and return its status and payload.
///
/// # Errors
/// A human-readable message if the socket cannot be reached, the framing is
/// broken, or the reply is malformed. (A reply that says *no* is not an error:
/// it comes back as a non-`Ok` [`Status`], which is what sets the exit code.)
pub fn send(request: &Request) -> Result<(Status, String), String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;
    set_timeouts(&stream, REPLY_TIMEOUT)
        .map_err(|e| format!("cannot set the socket timeout: {e}"))?;

    stream
        .write_all(&frame(&encode_request(request)))
        .map_err(|e| format!("cannot send the request: {e}"))?;

    let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
    stream
        .read_exact(&mut prefix)
        .map_err(|e| format!("rinit did not answer: {e}"))?;
    let len = frame_len(prefix).map_err(|e| format!("rinit sent a bad frame: {e}"))?;

    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .map_err(|e| format!("rinit's answer was truncated: {e}"))?;

    decode_response(&body).map_err(|e| format!("rinit's answer was malformed: {e}"))
}

/// Bound each direction of the exchange.
///
/// The timeout is per read/write, not for the whole reply: it exists to stop a
/// peer that goes silent from blocking the client indefinitely, not to put a
/// deadline on a large answer that is still arriving.
fn set_timeouts(stream: &UnixStream, timeout: Duration) -> std::io::Result<()> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))
}

/// The control socket path.
///
/// [`ENV_CONTROL_DIR`] overrides it, exactly as the server does, so a test can
/// point the client at a scratch socket.
fn socket_path() -> PathBuf {
    let dir = match std::env::var(ENV_CONTROL_DIR) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(rinit_core::CONTROL_DIR),
    };
    dir.join("control.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::time::Instant;

    /// The production timeout is 65 s; the same code path is given 50 ms here.
    #[test]
    fn a_reply_that_never_arrives_times_out() {
        let (mut reader, writer) = UnixStream::pair().expect("socket pair");
        set_timeouts(&reader, Duration::from_millis(50)).expect("set timeouts");

        let started = Instant::now();
        let mut byte = [0u8; 1];
        let err = reader
            .read_exact(&mut byte)
            .expect_err("nothing was written, so the read must not succeed");
        assert!(
            matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout did not fire"
        );
        drop(writer);
    }
}
