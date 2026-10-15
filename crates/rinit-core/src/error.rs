//! Core error types (plan §2.3 "错误层次").
//!
//! `CoreError` models pure-logic failures only: parsing, dependency resolution
//! and state-machine violations. It never carries a syscall errno - that is the
//! job of `rinit-runtime::SyscallError`.

use alloc::string::String;
use core::fmt;

/// An error produced by platform-independent rinit logic.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoreError {
    /// A unit name was empty or contained characters outside the permitted set.
    InvalidUnitName(String),
    // M1+ adds: Parse(ParseError), Dependency(DependencyError), State(StateError).
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::InvalidUnitName(name) => {
                write!(f, "invalid unit name: {name:?}")
            }
        }
    }
}

impl core::error::Error for CoreError {}
