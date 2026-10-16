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

extern crate alloc;

// The test harness (`cargo test`) needs `std`; re-add it for test builds only.
#[cfg(test)]
extern crate std;

pub mod control;
pub mod error;
pub mod unit;
pub mod users;

pub use control::{
    Command, LENGTH_PREFIX_BYTES, MAX_FRAME_BYTES, PROTOCOL_VERSION, ProtocolError, Request,
    Status, decode_request, decode_response, encode_request, encode_response, frame, frame_len,
};
pub use error::CoreError;
pub use unit::state::{Effect, Event, ExitOutcome, TransitionError, transition};
pub use unit::{
    DependencyError, DependencyGraph, ExecCommand, FailedReason, Job, JobQueue, JobTarget,
    NotifyError, NotifyMessage, ParseError, RequestSource, RestartPolicy, ServiceKind, UnitConfig,
    UnitName, UnitState, UnitType, parse_environment_file, parse_notify,
};
pub use users::{group_gid, passwd_uid};
