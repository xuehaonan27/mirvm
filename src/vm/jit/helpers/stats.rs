//! Helper call-frequency counters: one bucket per helper family, enabled process-wide by
//! `MIRVM_JIT_STATS=1` and dumped once at exit.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ===== helper frequency stats (MIRVM_JIT_STATS=1) =====
// Each helper entry does one fetch_add(Relaxed); at process exit a single line is dumped
// through libc atexit. With the knob off the cost is one relaxed load per entry.
pub(crate) static STAT_ON: AtomicBool = AtomicBool::new(false);
static STAT: [AtomicU64; 13] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
const STAT_NAMES: [&str; 13] = [
    "alloc",
    "tls_ref",
    "c2i",
    "call_indirect",
    "call_foreign",
    "call_builtin",
    "simd_stmt",
    "simd_rv",
    "volatile_load",
    "volatile_store",
    "call_terminate",
    "bin128_ovf",
    "syscall_trace",
];
pub(crate) const S_ALLOC: usize = 0;
pub(crate) const S_TLS: usize = 1;
pub(crate) const S_C2I: usize = 2;
pub(crate) const S_INDIR: usize = 3;
pub(crate) const S_FOREIGN: usize = 4;
pub(crate) const S_BUILTIN: usize = 5;
pub(crate) const S_SIMD_STMT: usize = 6;
pub(crate) const S_SIMD_RV: usize = 7;
pub(crate) const S_VLOAD: usize = 8;
pub(crate) const S_VSTORE: usize = 9;
pub(crate) const S_CTERM: usize = 10;
pub(crate) const S_BIN128: usize = 11;
/// The one bucket reachable only from trace-domain compiled code, so a non-zero
/// count is direct evidence that the pinned syscall site executed rather than the
/// interpreter's thread-local one.
pub(crate) const S_SYSCALL_TRACE: usize = 12;

#[inline(always)]
pub(crate) fn stat(i: usize) {
    if STAT_ON.load(Ordering::Relaxed) {
        STAT[i].fetch_add(1, Ordering::Relaxed);
    }
}

/// Frequency of one helper bucket. A test that must prove a path ran -- rather
/// than only that its effects match another path's -- reads it here.
#[cfg(test)]
pub(crate) fn stat_value(name: &str) -> u64 {
    let index = STAT_NAMES
        .iter()
        .position(|n| *n == name)
        .expect("unknown helper stat bucket");
    STAT[index].load(Ordering::Relaxed)
}

extern "C" fn stat_dump() {
    let mut line = String::from("mirvm-jit-stats:");
    for (i, n) in STAT_NAMES.iter().enumerate() {
        let v = STAT[i].load(Ordering::Relaxed);
        if v != 0 {
            line.push_str(&format!(" {n}={v}"));
        }
    }
    eprintln!("{line}");
}

/// Called once from `Compiler::new`: enables the stats from the environment and registers
/// the exit dump.
pub(crate) fn stat_init() {
    if crate::options::get().jit_stats {
        STAT_ON.store(true, Ordering::Relaxed);
        crate::os::process::atexit_native(stat_dump);
    }
}
