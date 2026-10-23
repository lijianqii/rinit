//! Integration test for the `rctl` binary against a fake server.
//!
//! The server half needs root (the real one is root-only), but the *client*
//! half - framing, decoding, printing, exit code - does not, so this drives the
//! real binary against a listener that replies by hand.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;

use rinit_core::{Request, Status, decode_request, encode_response, frame, frame_len};

/// A scratch directory for the socket, cleaned up at the end.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("rctl-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Read one framed request from `stream`.
fn read_request(stream: &mut UnixStream) -> Request {
    let mut prefix = [0u8; rinit_core::LENGTH_PREFIX_BYTES];
    stream.read_exact(&mut prefix).expect("read prefix");
    let len = frame_len(prefix).expect("length");
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).expect("read body");
    decode_request(&body).expect("decode request")
}

/// Reply with one framed message.
fn reply(stream: &mut UnixStream, status: Status, text: &str) {
    stream
        .write_all(&frame(&encode_response(status, text)))
        .expect("write reply");
    stream.flush().expect("flush");
}

/// Start a listener that answers exactly one request with `status`/`text`,
/// returning the request it saw.
fn serve_one(
    dir: &TempDir,
    status: Status,
    text: &'static str,
) -> (PathBuf, std::thread::JoinHandle<Request>) {
    let socket = dir.0.join("control.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let handle = std::thread::spawn(move || {
        let (mut stream, _addr) = listener.accept().expect("accept");
        let request = read_request(&mut stream);
        reply(&mut stream, status, text);
        request
    });
    (socket, handle)
}

fn run_rctl(dir: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rctl"))
        .args(args)
        .env("RINIT_CONTROL_DIR", &dir.0)
        .output()
        .expect("run rctl")
}

/// The happy path: the request is what the command line asked for, the reply is
/// printed, and the process exits 0.
#[test]
fn a_successful_command_prints_the_reply_and_exits_zero() {
    let dir = TempDir::new("ok");
    let (_socket, server) = serve_one(&dir, Status::Ok, "app.service: active");

    let output = run_rctl(&dir, &["status", "app.service"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("app.service: active"), "got: {stdout}");
    assert_eq!(
        server.join().expect("server"),
        Request::StatusFormatted {
            unit: Some(rinit_core::UnitName::parse("app.service").unwrap()),
            tail: 0,
            format: rinit_core::StatusFormat::Systemd
        }
    );
}

/// `rctl reload` asks the server to re-read the unit directory, and prints the
/// report it gets back (plan §7.6).
#[test]
fn reload_asks_the_server_to_reread_the_directory() {
    let dir = TempDir::new("reload");
    let (_, server) = serve_one(
        &dir,
        Status::Ok,
        "registered extra.service\nstarted extra.service: the boot target requires it\n\
         reload: 1 added (1 started), 0 changed (0 deferred), 0 removed (0 stopped), 3 unchanged",
    );

    let output = run_rctl(&dir, &["reload"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("registered extra.service"), "got: {stdout}");
    assert!(
        stdout.contains("1 added"),
        "the summary belongs in the reply: {stdout}"
    );
    assert_eq!(server.join().expect("server"), Request::Reload);
}

/// A reload the server refuses is an ordinary error: exit 1, and the reason -
/// which says the running configuration is untouched - on stderr.
#[test]
fn a_rejected_reload_is_reported_as_an_error() {
    let dir = TempDir::new("reload-rejected");
    let (_, server) = serve_one(
        &dir,
        Status::Error,
        "reload rejected, the running configuration is unchanged: app.service: not found",
    );

    let output = run_rctl(&dir, &["reload"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty(), "nothing belongs on stdout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unchanged"), "got: {stderr}");
    let _ = server.join();
}

/// A reply that is not `Ok` goes to stderr and sets the plan's exit code
/// (plan §3 M3: 3 = no such unit, 4 = conflict).
#[test]
fn a_failed_command_uses_the_status_exit_code() {
    let dir = TempDir::new("exit");
    let (_, server) = serve_one(&dir, Status::NoSuchUnit, "no such unit: ghost.service");

    let output = run_rctl(&dir, &["start", "ghost.service"]);

    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ghost.service"), "got: {stderr}");
    assert!(output.stdout.is_empty(), "nothing belongs on stdout");
    let _ = server.join();
}

/// A version mismatch is a hard failure with a message that says what to do.
#[test]
fn a_version_mismatch_is_reported() {
    let dir = TempDir::new("version");
    let (_, server) = serve_one(
        &dir,
        Status::VersionMismatch,
        "protocol version 1 does not match 2; rebuild rctl",
    );

    let output = run_rctl(&dir, &["list"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rebuild rctl"), "got: {stderr}");
    let _ = server.join();
}

/// No server at all is a general error, and the message names the socket.
#[test]
fn a_missing_server_is_an_error() {
    let dir = TempDir::new("missing");
    let output = run_rctl(&dir, &["list"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("control.sock"), "got: {stderr}");
}

/// A usage error is exit 2 and never touches the socket.
#[test]
fn a_usage_error_exits_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_rctl"))
        .args(["start"])
        .env(
            "RINIT_CONTROL_DIR",
            std::env::temp_dir().join("rctl-nowhere"),
        )
        .output()
        .expect("run rctl");

    assert_eq!(output.status.code(), Some(2));
}

/// `--help` and `--version` succeed without a server.
#[test]
fn help_and_version_need_no_server() {
    for args in [["--help"], ["--version"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_rctl"))
            .args(args)
            .env(
                "RINIT_CONTROL_DIR",
                std::env::temp_dir().join("rctl-nowhere"),
            )
            .output()
            .expect("run rctl");
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        assert!(!output.stdout.is_empty(), "{args:?}");
    }
}

/// A real request with no server behind it: the client says so on stderr and
/// exits 1, rather than panicking or hanging. That is what an operator sees when
/// rinit is not up, or when the control directory is not the one rinit used.
#[test]
fn a_request_with_no_server_is_reported_and_exits_one() {
    // The directory exists; nothing is listening in it.
    let dir = TempDir::new("no-server");

    let out = run_rctl(&dir, &["list"]);

    assert_eq!(out.status.code(), Some(1), "unexpected exit: {out:?}");
    assert!(out.stdout.is_empty(), "nothing to print: {out:?}");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("cannot connect"),
        "the error must say what failed: {text}"
    );
    assert!(
        text.contains("control.sock"),
        "and name the socket it tried: {text}"
    );
}
