//! Unit-file parser (plan §1.3(1)); lands with M1.
//!
//! M1 will add the parser here. It is specified as a **pure function**
//! (`&str -> Result<UnitConfig, ParseError>`) with no global state, which is
//! what makes it unit-testable in this crate, fuzzable per plan §4.4, and
//! reusable from the fake-runtime integration tests of plan §4.2.
//!
//! Format rules to implement (plan §1.3(1)):
//!
//! - Sections `[Unit]` and `[Service]`; `[Unit]` is optional, `[Service]` is
//!   required for services and absent for targets.
//! - `Key=Value` entries, `#` comments, and line continuation with a trailing
//!   `\`.
//! - An unknown `Key` in a known section is an error (fail loudly on typos in
//!   unit files, which are otherwise silent no-ops).
//! - An entry outside any section, a missing `=`, or a duplicate `Key` are all
//!   errors carrying the file name and line number.
//! - Unknown *sections* are an error too, so a stray `[Servcie]` is not ignored.
//!
//! Errors must point at the offending line, since a broken unit file is the
//! single most common operator error and the whole file must be diagnosable
//! from the message alone.
