//! rnet - a small IPv4 network manager for rinit systems.
//!
//! The crate is split into a strict configuration parser, an IPv4-only data
//! model, a netlink write path, a self-contained DHCPv4 client and a small
//! text control protocol. `rnetd` is the daemon; `rncli` is its client.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented
    )
)]

pub mod cli;
pub mod config;
pub mod control;
pub mod dhcp;
pub mod dns;
pub mod model;
pub mod netlink;
pub mod runtime;
pub mod state;

/// The running version, shared by both binaries.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default configuration path.
pub const DEFAULT_CONFIG: &str = "/etc/rnet/config";
/// Default control socket path.
pub const DEFAULT_CONTROL_SOCKET: &str = "/run/rnet/control.sock";
/// Default runtime directory.
pub const DEFAULT_RUN_DIR: &str = "/run/rnet";

/// Restore the default `SIGPIPE` disposition.
///
/// Rust ignores `SIGPIPE` by default, so a write to a closed pipe makes the
/// `println!`/`eprintln!` macro panic. That is wrong for a CLI (`rncli status |
/// head`) and for a daemon whose log reader went away. The rinit tools use the
/// same rule; this is the rnet equivalent.
pub fn restore_sigpipe() {
    // SAFETY: `signal` takes a signal number and a handler value; SIG_DFL is
    // the kernel's default action and has no user pointer.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
