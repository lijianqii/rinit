//! rinit-core - platform-independent core engine for rinit.
//!
//! This crate is deliberately `no_std + alloc` (plan §0.2, §2). It must not
//! depend on `std`, `libc`, or any platform syscall, so it can be unit-tested
//! in isolation and, if profiling ever demands it, dropped into a `#![no_main]`
//! binary unchanged. All state is passed explicitly: no `static mut`, no
//! `lazy_static` (plan §0.2 principle 2).
//!
//! M0 scope: error types and unit-name modelling. M1 adds the unit-file
//! parser, the dependency graph and the unit data model; the state machine and
//! manager land in M2 (plan §3).
#![no_std]
#![forbid(unsafe_code)]
// Production code must not panic. rinit does not unwind, and a panic in PID 1
// is a kernel panic at best and a hung machine at worst, so an `unwrap` on a
// "cannot happen" path is not a cheap assertion - it is the most expensive
// possible failure mode (plan §3 M4, the panic-free production path).
//
// `cfg_attr(not(test), ...)` keeps the test modules - where `unwrap` is how you
// assert - out of it. The few places where a bound is genuinely proven carry a
// targeted `#[allow]` with the reasoning next to it.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]

extern crate alloc;

// The test harness (`cargo test`) needs `std`; re-add it for test builds only.
#[cfg(test)]
extern crate std;

pub mod control;
pub mod error;
pub mod logger;
pub mod unit;
pub mod users;

pub use control::{
    CONTROL_DIR, CONTROL_SOCKET, Command, ENV_CONTROL_DIR, LENGTH_PREFIX_BYTES, MAX_FRAME_BYTES,
    PROTOCOL_VERSION, ProtocolError, Request, Status, decode_request, decode_response,
    encode_request, encode_response, frame, frame_len,
};
pub use error::CoreError;
pub use logger::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINE_BYTES, LogBuffer, LogLine};
pub use unit::state::{Effect, Event, ExitOutcome, TransitionError, transition};
pub use unit::{
    DependencyError, DependencyGraph, ExecCommand, FailedReason, Job, JobQueue, JobTarget,
    NotifyError, NotifyMessage, ParseError, RequestSource, RestartPolicy, ServiceKind, UnitConfig,
    UnitName, UnitState, UnitType, parse_environment_file, parse_notify,
};
pub use users::{group_gid, passwd_uid};
