//! Isolated decoder cost and allocation probe, copied into a disposable test build.

use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::time::Instant;

thread_local! {
    static COUNTS: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
}

struct Counted;

fn allocated(size: usize) {
    let _ = COUNTS.try_with(|counts| {
        if let Some((calls, bytes)) = counts.get() {
            counts.set(Some((calls + 1, bytes + size as u64)));
        }
    });
}

// SAFETY: allocation and deallocation delegate unchanged to System; counters retain no pointers.
unsafe impl GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counted = Counted;

fn allocation<T>(run: &impl Fn() -> T) -> (u64, u64) {
    COUNTS.with(|counts| counts.set(Some((0, 0))));
    let value = run();
    let result = COUNTS.with(|counts| counts.replace(None).unwrap());
    black_box(value);
    result
}

fn elapsed<T>(run: &impl Fn() -> T) -> u128 {
    let start = Instant::now();
    black_box(run());
    start.elapsed().as_nanos()
}

fn compare<T>(name: &str, before: impl Fn() -> T, after: impl Fn() -> T) {
    let old_allocations = allocation(&before);
    let new_allocations = allocation(&after);
    let (mut old, mut new) = (Vec::new(), Vec::new());
    for round in 0..32 {
        if round % 2 == 0 {
            old.push(elapsed(&before));
            new.push(elapsed(&after));
        } else {
            new.push(elapsed(&after));
            old.push(elapsed(&before));
        }
    }
    println!(
        "{}",
        serde_json::json!({
            "case": name, "before_ns": old, "after_ns": new,
            "before_allocations": old_allocations, "after_allocations": new_allocations,
        })
    );
}

#[test]
#[ignore = "explicit decoder profiling input required"]
fn preparation_decoders_profile() {
    let path = std::env::var_os("PREPARATION_INDEX_PATH").expect("index JSON-lines path");
    let text = std::fs::read_to_string(path).unwrap();
    let before = || {
        let mut result = Vec::new();
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let value = serde_json::from_str(line).unwrap();
            result.push(parse_index_version(&value).unwrap());
        }
        result
    };
    let after = || parse_index_lines(black_box(&text)).unwrap();
    assert_eq!(format!("{:?}", before()), format!("{:?}", after()));
    println!(
        "index_bytes={} index_blake3={}",
        text.len(),
        blake3::hash(text.as_bytes())
    );
    compare("registry-index", before, after);
    for size in [16 * 1024, 1024 * 1024] {
        let bytes: Vec<u8> = (0..size).map(|i| i as u8).collect();
        let encoded = postcard::to_stdvec(&bytes).unwrap();
        let before = || postcard::from_bytes::<Vec<u8>>(black_box(&encoded)).unwrap();
        let after = || {
            postcard::from_bytes::<crate::vm::frozen::SnapshotBytes>(black_box(&encoded))
                .unwrap()
                .0
        };
        assert_eq!(before(), after());
        compare(&format!("snapshot-{size}"), before, after);
    }
}
