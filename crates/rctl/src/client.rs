//! The socket half of `rctl`: connect, send one request, read one reply.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use rinit_core::{
    ENV_CONTROL_DIR, LENGTH_PREFIX_BYTES, Request, Status, decode_response, encode_request, frame,
    frame_len,
};

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
