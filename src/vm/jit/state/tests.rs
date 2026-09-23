//! Slot publication and perf-map lifecycle tests: the table a compile publishes into, and the
//! registration that must never replace a map that is already there.

use super::{
    JitState, JitSymbolRange, JitSymbolRole, PerfMapHealth, PerfMapRegistry, install_registry,
    stop_registry, with_registry,
};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

fn test_map_path(name: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "mirvm-{name}-{}-{}.map",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn range(func: u32, role: JitSymbolRole, start: u64, size: u64) -> JitSymbolRange {
    JitSymbolRange::new(17, func, role, start, size, "guest\nname")
}

#[test]
fn tables_sized_to_fn_count_and_zero_initialized() {
    let j = JitState::new(7);
    assert_eq!(j.slots.len(), 7);
    assert_eq!(j.counters.len(), 7);
    assert!(j.slots.iter().all(|s| s.load(Ordering::Acquire) == 0));
    assert!(j.worker.lock().unwrap().is_none());
    assert!(!j.stopping.load(Ordering::Acquire));
    assert!(j.threshold > 0);
    assert!(j.guest_code.read().unwrap().is_empty());
    assert!(j.symbol_ranges().is_empty());
}

#[test]
fn compiled_entry_publication_registers_all_roles_before_slots_become_visible() {
    let registry = Mutex::new(PerfMapRegistry::default());
    let path = test_map_path("jit-publish");
    install_registry(&registry, &path).unwrap();

    let j = JitState::new(4);
    j.publish_compiled_entries_with(
        super::CodeDomain::Plain,
        3,
        0x2000,
        0x3000,
        vec![
            range(3, JitSymbolRole::FastBody, 0x1000, 0x31),
            range(3, JitSymbolRole::Guarded, 0x2000, 0x17),
            range(3, JitSymbolRole::Packed, 0x3000, 0x29),
        ],
        &registry,
    );
    j.publish_c2i_entry_with(
        super::CodeDomain::Plain,
        2,
        0x4000,
        vec![range(2, JitSymbolRole::C2i, 0x4000, 0x1d)],
        &registry,
    );

    assert_eq!(j.slots_fast[3].load(Ordering::Acquire), 0x2000);
    assert_eq!(j.slots[3].load(Ordering::Acquire), 0x3000);
    assert_eq!(j.slots_fast[2].load(Ordering::Acquire), 0x4000);
    let ranges = j.symbol_ranges();
    assert_eq!(ranges.len(), 4);
    assert_eq!(
        with_registry(&registry, |registry| registry.ranges.len()),
        4
    );
    for role in [
        JitSymbolRole::FastBody,
        JitSymbolRole::Guarded,
        JitSymbolRole::Packed,
        JitSymbolRole::C2i,
    ] {
        assert!(ranges.iter().any(|range| range.role == role));
    }
    assert_eq!(j.guest_func_at(0x1010), Some(3));
    assert_eq!(j.guest_func_at(0x2010), None);
    assert_eq!(j.guest_func_at(0x3010), None);
    assert_eq!(j.guest_func_at(0x4010), None);

    assert_eq!(std::fs::read(&path).unwrap(), b"");
    let active = with_registry(&registry, |registry| registry.status());
    assert_eq!(active.health, PerfMapHealth::Active);
    assert_eq!(active.registered_ranges, 4);
    assert_eq!(active.written_ranges, 0);

    let stopped = stop_registry(&registry);
    assert_eq!(stopped.health, PerfMapHealth::Inactive);
    assert_eq!(stopped.written_ranges, 4);
    let map = std::fs::read_to_string(&path).unwrap();
    assert!(map.contains("1000 31 mirvm::engine-17::func-3::fast-body::guest\\nname\n"));
    assert!(map.contains("2000 17 mirvm::engine-17::func-3::guarded::guest\\nname\n"));
    assert!(map.contains("3000 29 mirvm::engine-17::func-3::packed::guest\\nname\n"));
    assert!(map.contains("4000 1d mirvm::engine-17::func-2::c2i::guest\\nname\n"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn map_open_failure_is_incomplete_but_does_not_block_entry_publication() {
    let registry = Mutex::new(PerfMapRegistry::default());
    let missing_parent = test_map_path("missing-parent");
    let path = missing_parent.join("perf.map");
    assert!(install_registry(&registry, &path).is_err());
    assert_eq!(
        with_registry(&registry, |registry| registry.status().health),
        PerfMapHealth::Incomplete
    );

    let j = JitState::new(1);
    j.publish_c2i_entry_with(
        super::CodeDomain::Plain,
        0,
        0x5000,
        vec![range(0, JitSymbolRole::C2i, 0x5000, 0x20)],
        &registry,
    );
    assert_eq!(j.slots_fast[0].load(Ordering::Acquire), 0x5000);
    assert_eq!(j.symbol_ranges().len(), 1);
    assert_eq!(
        with_registry(&registry, |registry| registry.status().health),
        PerfMapHealth::Incomplete
    );
}

#[test]
fn install_never_replaces_an_existing_or_active_map() {
    let registry = Mutex::new(PerfMapRegistry::default());
    let existing = test_map_path("jit-existing");
    std::fs::write(&existing, b"existing\n").unwrap();
    let error = install_registry(&registry, &existing).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&existing).unwrap(), b"existing\n");
    assert_eq!(
        with_registry(&registry, |registry| registry.status().health),
        PerfMapHealth::Incomplete
    );
    std::fs::remove_file(existing).unwrap();

    let active = test_map_path("jit-active");
    install_registry(&registry, &active).unwrap();
    let other = test_map_path("jit-second-active");
    let error = install_registry(&registry, &other).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(!other.exists());
    assert_eq!(
        with_registry(&registry, |registry| registry.status().health),
        PerfMapHealth::Active
    );
    stop_registry(&registry);
    std::fs::remove_file(active).unwrap();
}

#[test]
fn inactive_registration_is_written_only_by_explicit_stop() {
    let registry = Mutex::new(PerfMapRegistry::default());
    let j = JitState::new(1);
    j.publish_c2i_entry_with(
        super::CodeDomain::Plain,
        0,
        0x6000,
        vec![range(0, JitSymbolRole::C2i, 0x6000, 0x21)],
        &registry,
    );
    let before = with_registry(&registry, |registry| registry.status());
    assert_eq!(before.health, PerfMapHealth::Inactive);
    assert_eq!(before.path, None);
    assert_eq!(before.registered_ranges, 1);
    assert_eq!(before.written_ranges, 0);

    let path = test_map_path("jit-backfill");
    let after = install_registry(&registry, &path).unwrap();
    assert_eq!(after.health, PerfMapHealth::Active);
    assert_eq!(after.written_ranges, 0);
    assert_eq!(std::fs::read(&path).unwrap(), b"");
    let stopped = stop_registry(&registry);
    assert_eq!(stopped.health, PerfMapHealth::Inactive);
    assert_eq!(stopped.written_ranges, 1);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "6000 21 mirvm::engine-17::func-0::c2i::guest\\nname\n"
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn concurrent_engines_append_complete_lines() {
    let registry = std::sync::Arc::new(Mutex::new(PerfMapRegistry::default()));
    let path = test_map_path("jit-concurrent");
    install_registry(&registry, &path).unwrap();
    let mut workers = Vec::new();
    for engine_id in 1..=8 {
        let registry = std::sync::Arc::clone(&registry);
        workers.push(std::thread::spawn(move || {
            let j = JitState::new(1);
            let start = 0x7000 + engine_id * 0x100;
            let range =
                JitSymbolRange::new(engine_id, 0, JitSymbolRole::C2i, start, 0x22, "target");
            j.publish_c2i_entry_with(super::CodeDomain::Plain, 0, start, vec![range], &registry);
            assert_eq!(j.slots_fast[0].load(Ordering::Acquire), start);
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(std::fs::read(&path).unwrap(), b"");
    assert_eq!(
        with_registry(&registry, |registry| registry.status().written_ranges),
        0
    );
    assert_eq!(stop_registry(&registry).written_ranges, 8);

    let map = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<_> = map.lines().collect();
    assert_eq!(lines.len(), 8);
    for engine_id in 1..=8 {
        let start = 0x7000 + engine_id * 0x100;
        assert!(lines.iter().any(|line| {
            *line == format!("{start:x} 22 mirvm::engine-{engine_id}::func-0::c2i::target")
        }));
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn stop_cutoff_excludes_ranges_published_after_its_linearization_point() {
    struct BlockingSink {
        gate: std::sync::Arc<(Mutex<(bool, bool)>, std::sync::Condvar)>,
        bytes: std::sync::Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for BlockingSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (state, changed) = &*self.gate;
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.0 = true;
            changed.notify_all();
            while !state.1 {
                state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            drop(state);
            self.bytes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let registry = std::sync::Arc::new(Mutex::new(PerfMapRegistry::default()));
    let j = JitState::new(2);
    j.publish_c2i_entry_with(
        super::CodeDomain::Plain,
        0,
        0x9000,
        vec![range(0, JitSymbolRole::C2i, 0x9000, 0x24)],
        &registry,
    );
    let gate = std::sync::Arc::new((Mutex::new((false, false)), std::sync::Condvar::new()));
    let bytes = std::sync::Arc::new(Mutex::new(Vec::new()));
    with_registry(&registry, |registry| {
        registry.sink = Some(Box::new(BlockingSink {
            gate: std::sync::Arc::clone(&gate),
            bytes: std::sync::Arc::clone(&bytes),
        }));
        registry.health = PerfMapHealth::Active;
        registry.path = Some(PathBuf::from("blocking.map"));
    });

    let stop_registry_ref = std::sync::Arc::clone(&registry);
    let stop = std::thread::spawn(move || stop_registry(&stop_registry_ref));
    let (gate_state, changed) = &*gate;
    let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
    while !flags.0 {
        flags = changed.wait(flags).unwrap_or_else(|e| e.into_inner());
    }
    drop(flags);

    let operation = with_registry(&registry, |registry| {
        registry
            .control
            .clone()
            .expect("the first stop must remain in progress")
    });
    let second_registry = std::sync::Arc::clone(&registry);
    let second_stop = std::thread::spawn(move || stop_registry(&second_registry));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while operation.waiters.load(Ordering::SeqCst) == 0 {
        assert!(
            !second_stop.is_finished(),
            "a concurrent stop returned an intermediate status"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the concurrent stop did not join the active control operation"
        );
        std::thread::yield_now();
    }
    let next = test_map_path("jit-next-session");
    let next_for_install = next.clone();
    let install_registry_ref = std::sync::Arc::clone(&registry);
    let install =
        std::thread::spawn(move || install_registry(&install_registry_ref, &next_for_install));
    while operation.waiters.load(Ordering::SeqCst) < 2 {
        assert!(
            !install.is_finished(),
            "an install raced past the active stop operation"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "the concurrent install did not wait for stop"
        );
        std::thread::yield_now();
    }

    j.publish_c2i_entry_with(
        super::CodeDomain::Plain,
        1,
        0xa000,
        vec![range(1, JitSymbolRole::C2i, 0xa000, 0x25)],
        &registry,
    );
    let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
    flags.1 = true;
    changed.notify_all();
    drop(flags);

    let status = stop.join().unwrap();
    let second_status = second_stop.join().unwrap();
    assert_eq!(second_status, status);
    assert_eq!(
        install.join().unwrap().unwrap().health,
        PerfMapHealth::Active
    );
    assert_eq!(status.health, PerfMapHealth::Inactive);
    assert_eq!(status.registered_ranges, 2);
    assert_eq!(status.written_ranges, 1);
    let first = String::from_utf8(bytes.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap();
    assert!(first.contains("9000 24 mirvm::engine-17::func-0::c2i"));
    assert!(!first.contains("func-1::c2i"));

    assert_eq!(stop_registry(&registry).written_ranges, 2);
    let next_map = std::fs::read_to_string(&next).unwrap();
    assert!(next_map.contains("func-0::c2i"));
    assert!(next_map.contains("func-1::c2i"));
    std::fs::remove_file(next).unwrap();
}

#[test]
fn concurrent_stop_waiter_observes_the_same_write_failure() {
    struct BlockingFailingSink {
        gate: std::sync::Arc<(Mutex<(bool, bool)>, std::sync::Condvar)>,
        writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl std::io::Write for BlockingFailingSink {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            let (state, changed) = &*self.gate;
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.0 = true;
            changed.notify_all();
            while !state.1 {
                state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            Err(std::io::Error::other("injected blocked write failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let registry = std::sync::Arc::new(Mutex::new(PerfMapRegistry::default()));
    let gate = std::sync::Arc::new((Mutex::new((false, false)), std::sync::Condvar::new()));
    let writes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    with_registry(&registry, |registry| {
        registry.sink = Some(Box::new(BlockingFailingSink {
            gate: std::sync::Arc::clone(&gate),
            writes: std::sync::Arc::clone(&writes),
        }));
        registry.health = PerfMapHealth::Active;
        registry.path = Some(PathBuf::from("blocking-failure.map"));
    });
    let j = JitState::new(1);
    j.publish_c2i_entry_with(
        super::CodeDomain::Plain,
        0,
        0xb000,
        vec![range(0, JitSymbolRole::C2i, 0xb000, 0x26)],
        &registry,
    );

    let first_registry = std::sync::Arc::clone(&registry);
    let first = std::thread::spawn(move || stop_registry(&first_registry));
    let (gate_state, changed) = &*gate;
    let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
    while !flags.0 {
        flags = changed.wait(flags).unwrap_or_else(|e| e.into_inner());
    }
    drop(flags);

    let operation = with_registry(&registry, |registry| {
        registry
            .control
            .clone()
            .expect("the failing stop must remain in progress")
    });
    let second_registry = std::sync::Arc::clone(&registry);
    let second = std::thread::spawn(move || stop_registry(&second_registry));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while operation.waiters.load(Ordering::SeqCst) == 0 {
        assert!(!second.is_finished(), "the second stop returned too early");
        assert!(
            std::time::Instant::now() < deadline,
            "the second stop did not join the failing operation"
        );
        std::thread::yield_now();
    }

    let mut flags = gate_state.lock().unwrap_or_else(|e| e.into_inner());
    flags.1 = true;
    changed.notify_all();
    drop(flags);

    let first_status = first.join().unwrap();
    let second_status = second.join().unwrap();
    assert_eq!(second_status, first_status);
    assert_eq!(first_status.health, PerfMapHealth::Incomplete);
    assert!(
        first_status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("injected blocked write failure"))
    );
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}

#[test]
fn stop_write_failure_is_incomplete_without_blocking_published_code() {
    struct FailingSink;

    impl std::io::Write for FailingSink {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected perf-map write failure"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let registry = Mutex::new(PerfMapRegistry::default());
    with_registry(&registry, |registry| {
        registry.sink = Some(Box::new(FailingSink));
        registry.health = PerfMapHealth::Active;
        registry.path = Some(PathBuf::from("injected.map"));
    });
    let j = JitState::new(1);
    j.publish_c2i_entry_with(
        super::CodeDomain::Plain,
        0,
        0x8000,
        vec![range(0, JitSymbolRole::C2i, 0x8000, 0x23)],
        &registry,
    );

    assert_eq!(j.slots_fast[0].load(Ordering::Acquire), 0x8000);
    let status = stop_registry(&registry);
    assert_eq!(status.health, PerfMapHealth::Incomplete);
    assert_eq!(status.registered_ranges, 1);
    assert_eq!(status.written_ranges, 0);
    assert!(
        status
            .error
            .as_deref()
            .is_some_and(|error| error.contains("injected perf-map write failure"))
    );
}
/// Plain and trace publish slots must be fully independent. A trace body
/// assumes recorder state the plain domain does not have, so an address
/// published for one domain must never become visible to the other -- and the
/// selection itself must be a view, not a copy, so the plain path keeps using
/// the `slots`/`slots_fast` fields.
#[test]
fn plain_and_trace_publish_slots_are_independent() {
    let jit = super::JitState::new(3);
    let plain = jit.slots_for(super::CodeDomain::Plain);
    let trace = jit.slots_for(super::CodeDomain::Trace);
    assert_eq!(plain.slots.len(), 3);
    assert_eq!(trace.slots.len(), 3);
    assert!(
        !std::ptr::eq(plain.slots, trace.slots),
        "the two domains must not share a publish slot array"
    );

    // Publishing into the trace domain leaves the plain entries untouched.
    trace.slots[1].store(0xabcd, Ordering::Release);
    assert_eq!(jit.slots[1].load(Ordering::Acquire), 0);
    assert_eq!(jit.trace.slots[1].load(Ordering::Acquire), 0xabcd);

    // And the plain view still borrows those same fields.
    plain.slots[2].store(0x1234, Ordering::Release);
    assert_eq!(jit.slots[2].load(Ordering::Acquire), 0x1234);
}
