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

// ===== JIT code store counters =====
// The observable half of "any doubt is a miss": what the store answered, what it did not hold, and what
// it held and could not be used. Counted on the compile worker's cold path, so the cost is one relaxed
// increment per compile or lookup, and dumped with the helper buckets.
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
static CACHE_REFUSED: AtomicU64 = AtomicU64::new(0);
static CACHE_PRELINKED: AtomicU64 = AtomicU64::new(0);
static TIER_BASELINE: AtomicU64 = AtomicU64::new(0);
static TIER_OPTIMIZED: AtomicU64 = AtomicU64::new(0);

/// An entry was linked and published from the store.
pub(crate) fn cache_hit() {
    CACHE_HITS.fetch_add(1, Ordering::Relaxed);
}

/// The store held nothing for this fragment and key.
pub(crate) fn cache_miss() {
    CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
}

/// The store held something that could not be used, which is why the function was compiled.
pub(crate) fn cache_refused() {
    CACHE_REFUSED.fetch_add(1, Ordering::Relaxed);
}

/// Entries linked from the store before any request was served: what warmup traded a compile wave for.
pub(crate) fn cache_prelinked(count: u64) {
    CACHE_PRELINKED.fetch_add(count, Ordering::Relaxed);
}

/// Functions this session answered at the cheap tier, and at the optimized one. The split is what the
/// adaptive policy is judged by.
pub(crate) fn tier_baseline() {
    TIER_BASELINE.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn tier_optimized() {
    TIER_OPTIMIZED.fetch_add(1, Ordering::Relaxed);
}

/// The store counters, for a diagnostic that must name them.
fn cache_counts() -> (u64, u64, u64) {
    (
        CACHE_HITS.load(Ordering::Relaxed),
        CACHE_MISSES.load(Ordering::Relaxed),
        CACHE_REFUSED.load(Ordering::Relaxed),
    )
}

extern "C" fn stat_dump() {
    let mut line = String::from("mirvm-jit-stats:");
    for (i, n) in STAT_NAMES.iter().enumerate() {
        let v = STAT[i].load(Ordering::Relaxed);
        if v != 0 {
            line.push_str(&format!(" {n}={v}"));
        }
    }
    let (hits, misses, refused) = cache_counts();
    if hits + misses + refused != 0 {
        line.push_str(&format!(
            " cache_hits={hits} cache_misses={misses} cache_refused={refused} \
             cache_prelinked={} tier_baseline={} tier_optimized={}",
            CACHE_PRELINKED.load(Ordering::Relaxed),
            TIER_BASELINE.load(Ordering::Relaxed),
            TIER_OPTIMIZED.load(Ordering::Relaxed)
        ));
    }
    eprintln!("{line}");
}

/// Called from `Compiler::new`, which one Engine does once per tier: the environment is read and the
/// exit dump registered exactly once per process, so the counters are not printed twice.
pub(crate) fn stat_init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if crate::options::jit_stats() {
            STAT_ON.store(true, Ordering::Relaxed);
            crate::os::process::atexit_native(stat_dump);
        }
        if crate::options::jit_ledger() {
            LEDGER_ON.store(true, Ordering::Relaxed);
            crate::os::process::atexit_native(ledger_dump);
        }
    });
}

// ===== the compile-time ledger (MIRVM_JIT_LEDGER=1) =====
// What the pre-linking floor and the tier thresholds are priced from: one row per function this
// process built or linked out of the JIT store, with the two sizes that decide whether linking a
// small body is cheaper than compiling it. Measurement only — the knob is off unless asked for, and
// the row costs one lock per compile or link, which is nothing beside either.
static LEDGER_ON: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Built {
    Compiled,
    Linked,
}

impl Built {
    fn name(self) -> &'static str {
        match self {
            Built::Compiled => "compiled",
            Built::Linked => "linked",
        }
    }
}

/// One measured function: how it was produced, the canonical body's size, the machine code it became,
/// and how long the producing call took.
struct Row {
    built: Built,
    body: u64,
    code: u64,
    micros: u64,
}

static LEDGER: std::sync::Mutex<Vec<Row>> = std::sync::Mutex::new(Vec::new());

/// Whether the ledger is on, so a caller can skip the work that only feeds it.
#[inline]
pub(crate) fn ledger_on() -> bool {
    LEDGER_ON.load(Ordering::Relaxed)
}

pub(crate) fn ledger_row(built: Built, body: u64, code: u64, micros: u64) {
    if let Ok(mut rows) = LEDGER.lock() {
        rows.push(Row {
            built,
            body,
            code,
            micros,
        });
    }
}

extern "C" fn ledger_dump() {
    let Ok(mut rows) = LEDGER.lock() else {
        return;
    };
    // One summary per kind, then the rows by body size: the floor is the size where the two per-kind
    // costs cross, so the sizes have to be comparable across rows.
    for kind in [Built::Compiled, Built::Linked] {
        let (mut count, mut body, mut code, mut micros) = (0u64, 0u64, 0u64, 0u64);
        for row in rows.iter().filter(|row| row.built == kind) {
            count += 1;
            body += row.body;
            code += row.code;
            micros += row.micros;
        }
        if count == 0 {
            continue;
        }
        eprintln!(
            "mirvm-jit-ledger: {} count={count} body_bytes={body} code_bytes={code} micros={micros} \
             body_per_us={:.1} code_per_body={:.2}",
            kind.name(),
            body as f64 / micros.max(1) as f64,
            code as f64 / body.max(1) as f64,
        );
    }
    rows.sort_by_key(|row| row.body);
    for row in rows.iter() {
        eprintln!(
            "mirvm-jit-ledger-row: {} body={} code={} micros={}",
            row.built.name(),
            row.body,
            row.code,
            row.micros
        );
    }
}
