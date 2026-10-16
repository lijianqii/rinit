//! Allocation balance for `rinit-core`'s pure logic (plan §4.4).
//!
//! §4.4 asks for leak detection on the core with `miri`, because the core is
//! `no_std + alloc` pure logic and therefore the half of rinit that *can* be
//! checked that way. Neither miri (it needs a nightly toolchain) nor valgrind is
//! available in this environment, so this is the direct equivalent: every
//! allocation-owning path in the core is driven by a counting global allocator,
//! and the batch of live bytes must not grow between two identical runs.
//!
//! Comparing two *batches* rather than one run is what makes it robust: the test
//! harness allocates on its own account, and a single before/after snapshot
//! would blame it for that. Per-iteration leaks still show up, because 10,000
//! iterations turn one leaked byte into 10 KB - far more than any noise.

// Installing a `GlobalAlloc` is `unsafe` by definition, and this is the only
// file in the crate that needs to. The library is still `#![forbid(unsafe_code)]`
// (see `src/lib.rs`), which cannot be overridden - only this test target opts
// out, via the `deny` (not `forbid`) crate lint in `Cargo.toml`.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use rinit_core::unit::parse_unit_file;
use rinit_core::{
    DependencyGraph, Event, Job, JobQueue, JobTarget, LogBuffer, Request, RequestSource,
    RestartPolicy, ServiceKind, Status, UnitConfig, UnitName, UnitState, decode_request,
    decode_response, encode_request, encode_response, frame, frame_len, parse_environment_file,
    parse_notify, transition,
};

/// The allocator under test: a pass-through to the system allocator that counts
/// bytes still live.
struct Counting;

/// Bytes handed out and not yet given back, process-wide.
static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: every method forwards to `System`, which is the allocator the program
// would have used anyway; the counters are advisory and never affect the
// pointer or the layout.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The counters are process-wide, so the tests that measure them must not
/// overlap: libtest runs tests in parallel, and one test's allocations would be
/// charged to the other's batch.
static MEASURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`MEASURE_LOCK`], surviving a poisoned mutex (a failing test poisons it,
/// and the next test should still run rather than cascade).
fn measure() -> std::sync::MutexGuard<'static, ()> {
    MEASURE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A unit file that pulls on every allocation-owning directive at once:
/// dependency lists, four `Exec*=` commands, environment, user/group and a
/// health check.
const WORKER: &str = "\
[Unit]
Description=a worker that owns strings
After=db.service
Before=old.service
Wants=cache.service
Requires=db.service
Conflicts=old.service

[Service]
Type=simple
ExecStart=/bin/worker --flag value
ExecStartPre=/bin/pre
ExecStop=/bin/stop
Restart=on-failure
RestartSec=1
Environment=FOO=bar BAZ=qux
EnvironmentFile=/etc/worker.env
WorkingDirectory=/var/lib/worker
User=nobody
Group=nogroup
HealthCheck=/bin/health-check --probe
HealthIntervalSec=5
";

const DB: &str = "[Unit]\nDescription=db\n[Service]\nType=oneshot\nExecStart=/bin/db\n";
const CACHE: &str =
    "[Unit]\nDescription=cache\nAfter=db.service\n[Service]\nType=simple\nExecStart=/bin/cache\n";
const OLD: &str = "[Unit]\nDescription=old\n[Service]\nType=simple\nExecStart=/bin/old\n";

const ENV_FILE: &str = "# a comment\nA=1\nB=two\nC=three\n";
const NOTIFY: &[u8] = b"READY=1\nSTATUS=up and running\n";

/// One pass over everything in the core that owns memory.
fn workload() {
    let (worker, db, cache, old) = units();
    let configs = [&worker, &db, &cache, &old];

    // Dependency graph: adjacency lists, levels and a transitive closure.
    let graph = DependencyGraph::build(configs).expect("graph");
    let target = UnitName::parse("worker.service").expect("name");
    let _ = graph.units();
    let _ = graph.levels().expect("levels");
    let _ = graph.topological_order().expect("order");
    let _ = graph.dependencies_of(&target);
    let _ = graph.dependents_of(&target);
    let _ = graph.closure_of(&target);

    // Job queue: submission, merge and reconciliation.
    let mut queue = JobQueue::new();
    let mut at = 0;
    for unit in graph.units() {
        for (target, source) in [
            (JobTarget::Started, RequestSource::UserStart),
            (JobTarget::Stopped, RequestSource::UserStop),
            (JobTarget::Stopped, RequestSource::ShutdownStop),
        ] {
            queue.submit(Job::new(unit.clone(), target, source, at));
            at += 1;
        }
    }
    for unit in graph.units() {
        let _ = queue.target_of(unit);
        let _ = queue.reconcile(unit, UnitState::Active);
    }
    let _ = queue.targets();
    queue.forget(&target);

    // State machine: every kind/policy pair on a full lifecycle.
    let kinds = [
        ServiceKind::Simple,
        ServiceKind::Oneshot,
        ServiceKind::Notify,
    ];
    let policies = [RestartPolicy::No, RestartPolicy::Always];
    for kind in kinds {
        for policy in policies {
            let mut state = UnitState::Inactive;
            for event in [
                Event::Start,
                Event::StartUpComplete,
                Event::Stop,
                Event::StopComplete,
                Event::Exit(rinit_core::ExitOutcome::Success),
                Event::StartUpFailed,
                Event::BackoffElapsed,
                Event::StartLimitTripped,
                Event::ResetFailed,
            ] {
                if let Ok(next) = transition(state, event, policy, kind) {
                    state = next.to;
                }
            }
            let _ = rinit_core::unit::job::describe_failure(state);
        }
    }

    // Log ring: enough lines to force eviction and truncation.
    let mut logs = LogBuffer::with_defaults();
    for index in 0..64 {
        logs.push(
            &target,
            &format!("line {index} with a body that owns a String"),
        );
    }
    let _ = logs.tail(16);
    let _ = logs.tail_for(&target, 4);

    // Control protocol: both directions, framed and unframed.
    let body = encode_request(&Request::Log {
        unit: Some(target.clone()),
        tail: 5,
    });
    let _ = frame(&body);
    let _ = frame_len([0, 0, 0, 0]);
    let _ = decode_request(&body).expect("decode");
    let response = encode_response(Status::Ok, "a reply that owns a String");
    let _ = decode_response(&response).expect("decode");

    // The remaining parsers.
    let _ = parse_environment_file(ENV_FILE);
    let _ = parse_notify(NOTIFY).expect("notify");
    let _ = parse_unit_file(&target, WORKER);
}

/// Parse the four units.
fn units() -> (UnitConfig, UnitConfig, UnitConfig, UnitConfig) {
    let parse = |name: &str, text: &str| {
        parse_unit_file(&UnitName::parse(name).expect("name"), text).expect("parse")
    };
    (
        parse("worker.service", WORKER),
        parse("db.service", DB),
        parse("cache.service", CACHE),
        parse("old.service", OLD),
    )
}

/// The leak check: two identical batches of 10,000 workloads must end at the
/// same number of live bytes.
#[test]
fn the_core_workload_does_not_leak() {
    let _guard = measure();

    // A first batch warms up anything lazily initialised (interned strings, the
    // allocator's own bookkeeping) so the two measured batches are comparable.
    for _ in 0..BATCH {
        workload();
    }

    let after_first = live_bytes();
    for _ in 0..BATCH {
        workload();
    }
    let after_second = live_bytes();

    assert!(
        after_second <= after_first,
        "the core leaks {BATCH} workload(s) worth of memory: {after_first} live bytes \
         after the first batch, {after_second} after the second (a leak of one byte per \
         iteration would be {BATCH} bytes)"
    );
}

/// How many workloads per batch. The comparison is exact, so the batch only has
/// to be large enough that a per-iteration leak is unmistakable next to the
/// harness's own allocation noise - 5,000 iterations turn one leaked byte into
/// 5 KB. It also has to stay small enough that this stays cheap to run on every
/// commit.
const BATCH: usize = 5_000;

/// The log ring must be bounded in *memory*, not in the sum of its strings.
///
/// This is the test the M4 soak earned: a `LogBuffer` pushing 200,000 three-byte
/// lines used to hold 87,000 of them, which is about 11 MB of `LogLine` structs
/// behind a budget that claimed 256 KiB. `LINE_OVERHEAD` in the accounting is
/// what fixed it, and this is the number that says so.
#[test]
fn the_log_ring_is_bounded_in_memory() {
    let _guard = measure();
    let unit = UnitName::parse("churn.service").expect("name");
    let mut logs = LogBuffer::with_defaults();

    // Fill it, so the second batch starts from a full buffer.
    for index in 0..200_000 {
        logs.push(&unit, &format!("hi {index}"));
    }
    let full = live_bytes();
    assert!(
        logs.accounted_bytes() <= rinit_core::DEFAULT_MAX_BYTES,
        "accounted {} exceeds the budget",
        logs.accounted_bytes()
    );

    // A full buffer must not grow further, however much is pushed through it.
    for index in 0..200_000 {
        logs.push(&unit, &format!("hi {index}"));
    }
    let still_full = live_bytes();

    assert!(
        still_full <= full,
        "the ring keeps growing with its input: {full} live bytes after the first \
         200,000 lines, {still_full} after the second"
    );
    // A few hundred kilobytes, not tens of megabytes: the buffer, the lines and
    // allocator slack for a 256 KiB budget.
    let budget = rinit_core::DEFAULT_MAX_BYTES;
    assert!(
        still_full < 4 * budget as isize,
        "{still_full} live bytes for a {budget}-byte budget is not a memory bound"
    );
}

fn live_bytes() -> isize {
    LIVE.load(Ordering::Relaxed)
}
