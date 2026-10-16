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

/// The guard for a child-owning test.
///
/// Dropping it reaps whatever the test left behind. `on_child_exit` is
/// bookkeeping, not a `waitpid`, so a test that asserts on an exit without
/// reaping leaves a real zombie - which the *next* test's drain would then
/// collect, failing for an unrelated reason. Reaping here closes that off for
/// every test at once instead of relying on each one to remember.
pub struct ChildrenGuard {
    _inner: MutexGuard<'static, ()>,
}

impl Drop for ChildrenGuard {
    fn drop(&mut self) {
        // Still holding the lock: nothing else can be reaping concurrently.
        let _ = rinit_runtime::syscall::wait::reap_children();
    }
}

/// Take the guard from a synchronous test.
///
/// # Panics
/// Panics if called from inside an async runtime, which no synchronous test is.
pub fn children_lock() -> ChildrenGuard {
    ChildrenGuard {
        _inner: CHILDREN.blocking_lock(),
    }
}

/// Take the guard from an asynchronous test.
pub async fn children_lock_async() -> ChildrenGuard {
    ChildrenGuard {
        _inner: CHILDREN.lock().await,
    }
}
