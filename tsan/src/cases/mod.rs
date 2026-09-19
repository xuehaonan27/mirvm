//! Concurrency cases the TSan harness runs, one module per area of the engine.
//!
//! Every case is a `pub(crate) fn run_*() -> bool` that prints exactly one
//! `PASS <id> <description>` or `FAIL <id> <detail>` line. `IDS` is mirrored by `EXPECTED` in
//! `tests/suites/runtime/tsan.sh`, and the suite requires every id to appear as a `PASS` line:
//! a case that stops running cannot quietly look like a pass.
//!
//! A case must not introduce a guest data race: the verdict is "exit 0 and zero
//! `WARNING: ThreadSanitizer`", so any report is an engine bug, not a test artifact.
//! Concurrency *through engine APIs* (close racing a call, cross-thread signals) is the
//! point; unsynchronized guest memory is out of contract.
//!
//! `run_all(Some(id))` runs only that case, which is how you iterate on one: build with
//! `MIRVM_BUILD_ID=0 cargo run -Zbuild-std --target x86_64-unknown-linux-gnu --release -- <id>`.

pub(crate) mod capture_session;
pub(crate) mod close_race;
pub(crate) mod engine_core;
pub(crate) mod fork_guard;
pub(crate) mod guest_threads;
pub(crate) mod mixed_stack;
pub(crate) mod signals;

/// Case ids, in run order. Keep in sync with the case modules and with the suite.
pub(crate) const IDS: &[&str] = &[
    "mixed-stack-fib",
    "atomic-cross-tier",
    "blocking-io-liveness",
    "mixed-stack-unwind",
    "engine-atomics-thunk-cache",
    "capture-session-lifecycle",
    "engine-close-race",
    "guest-threads",
    "signal-delivery",
    "fork-guard",
];

/// The registry: id and entry point. Function pointers, so a filtered run calls exactly one
/// case -- building the list must not run anything.
const CASES: &[(&str, fn() -> bool)] = &[
    ("mixed-stack-fib", mixed_stack::run_mixed_stack_fib),
    ("atomic-cross-tier", mixed_stack::run_atomic_cross_tier),
    (
        "blocking-io-liveness",
        mixed_stack::run_blocking_io_liveness,
    ),
    ("mixed-stack-unwind", mixed_stack::run_mixed_stack_unwind),
    (
        "engine-atomics-thunk-cache",
        engine_core::run_engine_atomics_thunk_cache,
    ),
    (
        "capture-session-lifecycle",
        capture_session::run_capture_session_lifecycle,
    ),
    ("engine-close-race", close_race::run_close_race),
    ("guest-threads", guest_threads::run_guest_threads),
    ("signal-delivery", signals::run_signal_delivery),
    ("fork-guard", fork_guard::run_fork_guard),
];

/// Run every case, or just `only` when a case id is given. `false` means at least one failed.
pub(crate) fn run_all(only: Option<&str>) -> bool {
    let selected: Vec<(&str, fn() -> bool)> = CASES
        .iter()
        .filter(|(id, _)| only.is_none_or(|want| want == *id))
        .copied()
        .collect();
    if selected.is_empty() {
        println!(
            "tsan-harness: no case matches {only:?}; known ids: {}",
            IDS.join(", ")
        );
        return false;
    }
    let results: Vec<(&str, bool)> = selected.iter().map(|(id, run)| (*id, run())).collect();
    let failed: Vec<&str> = results
        .iter()
        .filter(|(_, ok)| !*ok)
        .map(|(id, _)| *id)
        .collect();
    let passed = results.len() - failed.len();
    if only.is_some() {
        println!(
            "tsan-harness: selected case {}",
            if failed.is_empty() { "PASS" } else { "FAIL" }
        );
    } else if failed.is_empty() {
        println!("tsan-harness: {passed}/{} cases PASS", IDS.len());
    } else {
        println!(
            "tsan-harness: {passed}/{} cases PASS; FAILED: {}",
            IDS.len(),
            failed.join(", ")
        );
    }
    failed.is_empty()
}
