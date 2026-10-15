//! Test-only coordination helpers.
//!
//! Compiled only for `cargo test` (`#[cfg(test)] mod test_support;` in `main`).

use tokio::sync::{Mutex, MutexGuard};

/// Serialises tests that spawn or reap children.
///
/// The reaper is `waitpid(-1, WNOHANG)`, which collects *any* child of the
/// process, and libtest runs the tests of one binary in parallel threads of
/// that process. Two child-owning tests would therefore steal each other's
/// exits: one test's drain can reap the child another is still waiting for, so
/// the "my child is gone" assertion fails for reasons unrelated to the code
/// under test. Every test that touches children takes this for its whole
/// duration.
///
/// A tokio mutex rather than a `std` one so that the async tests can hold it
/// across an `await` without tripping `clippy::await_holding_lock`.
pub static CHILDREN: Mutex<()> = Mutex::const_new(());

/// Take the guard from a synchronous test.
///
/// # Panics
/// Panics if called from inside an async runtime, which no synchronous test is.
pub fn children_lock() -> MutexGuard<'static, ()> {
    CHILDREN.blocking_lock()
}

/// Take the guard from an asynchronous test.
pub async fn children_lock_async() -> MutexGuard<'static, ()> {
    CHILDREN.lock().await
}
