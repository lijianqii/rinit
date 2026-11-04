//! Minimal control-socket client shared by reboot/poweroff.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use rinit_core::{
    ENV_CONTROL_DIR, LENGTH_PREFIX_BYTES, Request, Status, decode_response, encode_request, frame,
    frame_len,
};

const REPLY_TIMEOUT: Duration = Duration::from_secs(65);

fn socket_path() -> PathBuf {
    let dir = match std::env::var(ENV_CONTROL_DIR) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(rinit_core::CONTROL_DIR),
    };
    dir.join("control.sock")
}

/// Send one request and return the server's status and payload.
pub fn send(request: &Request) -> Result<(Status, String), String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;
    stream
        .set_read_timeout(Some(REPLY_TIMEOUT))
        .map_err(|e| format!("cannot set the socket timeout: {e}"))?;
    stream
        .set_write_timeout(Some(REPLY_TIMEOUT))
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

#[cfg(test)]
mod tests {
    use super::*;
    use rinit_core::{decode_request, encode_response};
    use std::os::unix::net::UnixListener;

    /// A complete request/reply over a real Unix socket. This covers the client
    /// half that the guest tools-test exercises only through qemu, and it is
    /// what makes a malformed reply path testable without a running rinit.
    #[test]
    fn send_round_trips_a_request() {
        let dir = std::env::temp_dir().join(format!("rinit-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("control.sock");
        let listener = UnixListener::bind(&path).expect("listener");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
            stream.read_exact(&mut prefix).expect("prefix");
            let len = frame_len(prefix).expect("length");
            let mut body = vec![0u8; len];
            stream.read_exact(&mut body).expect("body");
            assert!(matches!(decode_request(&body), Ok(Request::Reboot)));
            let response = encode_response(Status::Ok, "reboot requested");
            stream.write_all(&frame(&response)).expect("response");
        });

        // SAFETY: this test is the only reader of the control directory in the
        // library test binary; a unique directory keeps it isolated.
        unsafe { std::env::set_var(ENV_CONTROL_DIR, &dir) };
        let result = send(&Request::Reboot);
        // SAFETY: as above.
        unsafe { std::env::remove_var(ENV_CONTROL_DIR) };
        server.join().expect("server");
        assert_eq!(
            result.expect("reply"),
            (Status::Ok, "reboot requested".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
