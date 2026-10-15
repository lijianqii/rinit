//! Unit-file data model (plan §1.3(1)); lands with M1.
//!
//! M1 will define the `UnitConfig` produced by [`super::parser`] and consumed
//! by [`super::dependency`] and the unit manager. The model covers the
//! simplified format from plan §1.3(1):
//!
//! ```ini
//! [Unit]
//! Description=Example daemon
//! Requires=network.target
//! After=network.target
//!
//! [Service]
//! Type=simple
//! ExecStart=/usr/bin/example-daemon --foreground
//! Restart=on-failure
//! ```
//!
//! Design constraints carried into the implementation:
//!
//! - `Type` lives in the `[Service]` section and is one of
//!   `simple | oneshot | notify` (`forking` is deferred to plan §7.10, P2).
//! - Every optional directive needs an explicit "unset" representation, so the
//!   model must distinguish "no timeout" from "timeout of 0".
//! - The struct stays `no_std` + `alloc` and `no_std`-friendly (`core::time`
//!   durations or plain seconds), so `rinit-core` never needs `std`.
