#!/usr/bin/env mirvm
---
[dependencies]
# mimalloc 0.1.52 (newest 0.1.x) pulls libmimalloc-sys 0.1.49, the mimalloc C v3 line --
# the same source as the engine's own heap backend heap.rs but a different instance: two
# mimalloc allocators in one process, with guest pointers all coming from the guest arena.
# `extended` is used only for two deterministic anchors: mi_version() (a C library version
# constant, proving the vendored C library is linked and running) and mi_usable_size()
# (allocation granularity from the same size-class table, bit-deterministic across
# dimensions). stats_json() is not printed: it mixes startup bookkeeping with runtime arena state, whose allocation counts differ per dimension.
mimalloc = { version = "=0.1.52", features = ["extended"] }
---
// mimalloc 0.1 global-allocator replacement boundary probe: `#[global_allocator]` is set to
// CountingMi, a thin counting wrapper that delegates to MiMalloc, so every guest heap
// allocation crosses FFI into the vendored C mimalloc arena (mmap-backed native memory,
// not the engine built-in heap) and the engine heap-model boundary is stressed throughout.
// The point is pointer provenance: guest-side allocations must be released by the same
// allocator instance that produced them, and the engine must never hand a mimalloc pointer
// to its own heap (or vice versa), including when a value crosses a thread boundary.
//
// Test surface:
//   P0 anchor  : mi_version() (the C library version constant; the vendored C library must
//                actually be linked in and running, which the constant alone proves)
//   P1 vec     : 20k pushes growing a Vec (the realloc path) plus a 64-slot x 300-round wear
//                ring of vec![0; len] (alloc_zeroed, fresh alloc and interleaved drop) with a
//                zeroing-semantics check
//   P2 btree   : 10_000 keys from an LCG inserted into BTreeMap<u64, u64>, then key%3==0
//                removed and the remainder traversed and summed (the doomed keys are
//                collected before removal, so iteration order cannot affect the result)
//   P3 string  : push_str of 1536 fragments (String growth rate) plus 256 format! items
//                joined and shrink_to_fit
//   P4 align   : a Vec of repr(align(64)) elements, eight repr(align(4096)) page Boxes (the
//                alignment assertions must hold) and five (size, align) mi_usable_size()
//                granularity anchors: (24,8), (1000,16), (17,64), (65536,4096) and
//                (262144,32). Each is allocated through MiMalloc directly, so the counted
//                window must stay balanced.
//   P5 threads : std::thread::scope with 4 workers: payloads allocated on main are freed on
//                a worker (cross-thread remote free) and worker-allocated results are joined
//                back to main for deallocation (the reverse remote free), plus a fixed-size
//                workload per worker; checksums are combined in worker order, and only
//                interleaving-independent quantities (totals, sums, xor) are printed while
//                the peak watermark is printed only in the single-threaded phases
//   counting   : accounting happens only inside a phase window (the ENABLED gate), so
//                calls/alloc_bytes/peak/live are all window deltas; that isolates the startup
//                bookkeeping allocations, whose count differs across dimensions. Every phase
//                asserts live_delta == 0, i.e. every allocation is matched by a free.
//   remote     : a pointer allocated in one thread and freed in another is the operation the
//                engine heap model is least likely to expect; P5 covers it in both directions
//                and P3/P4 keep large objects alive across the window boundary.
//   provenance : the window only sees allocations that go through CountingMi; memory the C
//                library reserves for its own metadata is invisible and is not part of any
//                reported delta.
//   anchors    : per-phase FNV-1a and sums (u64 wrapping, bit-identical across dimensions)
// The ENABLED gate is a plain AtomicBool, so the accounting itself allocates nothing, and
// the startup allocation count (which depends on how the engine booted) is never part of any
// reported number.
//
// Determinism: the LCG is seeded, BTreeMap order is used, joins are combined in worker
// order, and there is no time, address or HashMap dependence. No floating point is used;
// stderr is empty (zero warnings); the output is roughly 20 lines. The line count is
// intentionally small: one report line per phase plus the mi_version line, the cumulative
// counters and the mimalloc_ok marker.
//
// Three-way rerun commands (from the repository root):
//   A: target/release/mirvm run tests/scripts/c_mimalloc.rs
//   B: cd $(grep -l 'name = "c_mimalloc"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/scripts/c_mimalloc.rs
//
// Constructor guard (why CFLAGS is set): libmimalloc.a (v3) has an
// `__attribute__((constructor)) mi_process_attach` in prim.c, i.e. one .init_array and one
// .fini_array entry. The engine native_archive rejects any static archive that carries
// constructor/destructor sections, exiting 101 with the engine's literal message "dlopen 生命周期语义尚未定义"
// test locks that discipline). The bypass injects `-DMI_PRIM_HAS_PROCESS_ATTACH` through
// CFLAGS, which compiles the ctor/dtor out; mimalloc lazy-initialising on its first
// allocation is the upstream-guaranteed path, and both dimensions build the same C library,
// so the differential stays fair. Gate wiring must inject that CFLAGS through the existing
// environment-injection channel; the default (no injection) refuses the archive by design,
// it is not a missing configuration. The commands that are actually run are therefore:
//   A: CFLAGS="-DMI_PRIM_HAS_PROCESS_ATTACH" target/release/mirvm run tests/scripts/c_mimalloc.rs
//   B: cd $(grep -l 'name = "c_mimalloc"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      CFLAGS="-DMI_PRIM_HAS_PROCESS_ATTACH" \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: CFLAGS="-DMI_PRIM_HAS_PROCESS_ATTACH" MIRVM_JIT_THRESHOLD=1 \
//      target/release/mirvm run tests/scripts/c_mimalloc.rs
//
// Allocation-routing contract for a custom global allocator: with `#[global_allocator]` set,
// the `__rust_*` allocation family must not be split across two heaps. The engine picks the
// builtin per lower session: the base/deps image bakes `__rust_*` into CallBuiltin(Rust*) on
// the engine heap in the Default session, while the delta/image program expands to the guest
// shims generated by the HIR expander, which call the user allocator. Left alone, the two
// heaps free each other pointers. The contract is that lowering registers the four shim
// FuncIds generated for a kind=Global module (Module.custom_alloc_shims) and that the
// interpreter CallBuiltin(Rust*) arm routes to them at run time, because allocation is
// program-level semantics independent of the session that baked the bytecode. Rebasing must
// remap those FuncIds by the same rule as exports/fn_addrs/sites; a missed remap calls a
// shifted id and the engine reports its literal "ABI 不匹配" (ABI mismatch) error after being asked for insert_entry or
// from_iter.
//
// Two minimal repros stay as regression sentinels: /tmp/ga_p_only.rs (a System wrapper whose
// main only printed "hello": stdout was fine and the exit path segfaulted while freeing
// across the two heaps) and /tmp/ga_vecstr.rs (a static pointer registry reported CROSS-FREE
// with a 6144-byte size for a buffer that was never handed out through the guest allocator).
// Both are expected to pass now. If a future change splits the allocation family again, the
// earliest symptom is a SIGSEGV or an unknown-pointer report while a phase window is open,
// not a wrong number.
use mimalloc::MiMalloc;
use std::alloc::{GlobalAlloc, Layout};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

// ===== counting allocator (delegates to MiMalloc; accounts inside a window) =====

struct CountingMi;

#[global_allocator]
static GLOBAL: CountingMi = CountingMi;

static ENABLED: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_BYTES: AtomicU64 = AtomicU64::new(0);

#[inline]
fn bump_peak(live: u64) {
    let mut peak = PEAK_BYTES.load(Relaxed);
    while live > peak {
        match PEAK_BYTES.compare_exchange_weak(peak, live, Relaxed, Relaxed) {
            Ok(_) => break,
            Err(p) => peak = p,
        }
    }
}

#[inline]
fn on_alloc(size: usize) {
    CALLS.fetch_add(1, Relaxed);
    ALLOC_BYTES.fetch_add(size as u64, Relaxed);
    let live = LIVE_BYTES.fetch_add(size as u64, Relaxed) + size as u64;
    bump_peak(live);
}

unsafe impl GlobalAlloc for CountingMi {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { MiMalloc.alloc(layout) };
        if !p.is_null() && ENABLED.load(Relaxed) {
            on_alloc(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { MiMalloc.alloc_zeroed(layout) };
        if !p.is_null() && ENABLED.load(Relaxed) {
            on_alloc(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { MiMalloc.dealloc(ptr, layout) };
        if ENABLED.load(Relaxed) {
            LIVE_BYTES.fetch_sub(layout.size() as u64, Relaxed);
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { MiMalloc.realloc(ptr, layout, new_size) };
        if !p.is_null() && ENABLED.load(Relaxed) {
            CALLS.fetch_add(1, Relaxed);
            let old = layout.size() as u64;
            let new = new_size as u64;
            if new >= old {
                ALLOC_BYTES.fetch_add(new - old, Relaxed);
                let live = LIVE_BYTES.fetch_add(new - old, Relaxed) + (new - old);
                bump_peak(live);
            } else {
                LIVE_BYTES.fetch_sub(old - new, Relaxed);
            }
        }
        p
    }
}

// ===== determinism helpers =====

fn fnv_mix(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn fnv_u64s(xs: &[u64]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &x in xs {
        fnv_mix(&mut h, &x.to_le_bytes());
    }
    h
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 ^ (self.0 >> 33)
    }
}

#[derive(Clone, Copy)]
struct Snap {
    calls: u64,
    bytes: u64,
    live: u64,
}

fn snap() -> Snap {
    Snap {
        calls: CALLS.load(Relaxed),
        bytes: ALLOC_BYTES.load(Relaxed),
        live: LIVE_BYTES.load(Relaxed),
    }
}

fn report(name: &str, s0: Snap, s1: Snap, peak: Option<u64>, extra: &str) {
    let live_delta = s1.live as i64 - s0.live as i64;
    assert_eq!(live_delta, 0, "{name}: unpaired allocation inside the window");
    let peak_s = match peak {
        Some(p) => format!(" peak={p}"),
        None => String::new(),
    };
    println!(
        "{name}: calls={} bytes={} live_delta={live_delta}{peak_s} {extra}",
        s1.calls - s0.calls,
        s1.bytes - s0.bytes
    );
}

// ===== phase P1: Vec growth plus wear =====

fn phase_vec() {
    ENABLED.store(true, Relaxed);
    let s0 = snap();

    // Growth chain (the realloc path)
    let mut vv: Vec<u64> = Vec::new();
    let mut l = Lcg(0x9e3779b97f4a7c15);
    for _ in 0..20_000 {
        vv.push(l.next());
    }
    let sum_growth: u64 = vv.iter().fold(0u64, |a, &x| a.wrapping_add(x));
    let fnv_growth = fnv_u64s(&vv);
    drop(vv);

    // Wear ring: 64 slots x 300 rounds, vec![0;len] (alloc_zeroed) + fixed refill + slot drop
    let mut slots: Vec<Option<Vec<u8>>> = (0..64).map(|_| None).collect();
    let mut fnv_ring = 0xcbf29ce484222325u64;
    let mut sum_ring: u64 = 0;
    for round in 0..300u64 {
        let len = 256 + ((round.wrapping_mul(37)) % 1024) as usize;
        let mut v = vec![0u8; len];
        for (i, b) in v.iter_mut().enumerate() {
            *b = (round as u8) ^ (i as u8) ^ 0xa5;
        }
        fnv_mix(&mut fnv_ring, &v[..len.min(64)]);
        sum_ring = sum_ring.wrapping_add(v.iter().map(|&b| b as u64).sum::<u64>());
        slots[(round % 64) as usize] = Some(v);
    }
    drop(slots);

    // Zeroing semantics: vec![0u16; 512] must be all zeros
    let z = vec![0u16; 512];
    assert_eq!(z.iter().map(|&x| x as u64).sum::<u64>(), 0, "zeroed semantics");
    drop(z); // freed in-window so it pairs up (otherwise live_delta counts it)

    let s1 = snap();
    ENABLED.store(false, Relaxed);
    let peak = PEAK_BYTES.swap(0, Relaxed);
    report(
        "vec",
        s0,
        s1,
        Some(peak),
        &format!("sum={sum_growth:#x} fnv={fnv_growth:#x} ring_sum={sum_ring:#x} ring_fnv={fnv_ring:#x}"),
    );
}

// ===== phase P2: BTreeMap with 10k keys =====

fn phase_btree() {
    ENABLED.store(true, Relaxed);
    let s0 = snap();

    let mut m: BTreeMap<u64, u64> = BTreeMap::new();
    let mut l = Lcg(0xdeadbeef12345678);
    for i in 0..10_000u64 {
        let k = l.next().wrapping_add(i << 1);
        m.insert(k, k.rotate_left(17) ^ i);
    }
    assert_eq!(m.len(), 10_000, "insert count");
    let hit = (0..1000u64)
        .filter(|&i| m.contains_key(&i))
        .count() as u64;

    // Remove key%3==0
    let doomed: Vec<u64> = m.keys().copied().filter(|k| k % 3 == 0).collect();
    let doomed_len = doomed.len();
    for k in doomed {
        assert!(m.remove(&k).is_some());
    }
    let mut sum_k: u64 = 0;
    let mut sum_v: u64 = 0;
    let mut fnv_rest = 0xcbf29ce484222325u64;
    for (&k, &v) in m.iter() {
        sum_k = sum_k.wrapping_add(k);
        sum_v = sum_v.wrapping_add(v);
        fnv_mix(&mut fnv_rest, &k.to_le_bytes());
        fnv_mix(&mut fnv_rest, &v.to_le_bytes());
    }
    let remain = m.len();
    drop(m);

    let s1 = snap();
    ENABLED.store(false, Relaxed);
    let peak = PEAK_BYTES.swap(0, Relaxed);
    report(
        "btree",
        s0,
        s1,
        Some(peak),
        &format!(
            "hit={hit} removed={doomed_len} remain={remain} sum_k={sum_k:#x} sum_v={sum_v:#x} fnv={fnv_rest:#x}"
        ),
    );
}

// ===== phase P3: String concatenation =====

fn phase_string() {
    ENABLED.store(true, Relaxed);
    let s0 = snap();

    const FRAGS: [&str; 8] = [
        "alpha", "beta-77", "γamma!", "delta.delta", "EPS", "z|z", "th-eta", "Ω",
    ];
    let mut s = String::new();
    for r in 0..1536usize {
        s.push_str(FRAGS[r % 8]);
        if r % 3 == 0 {
            s.push('|');
        }
    }
    let mut parts: Vec<String> = Vec::new();
    for i in 0..256u64 {
        parts.push(format!("k{i:05}={}", i.wrapping_mul(2654435761) % 1000003));
    }
    let joined = parts.join("|");
    s.shrink_to_fit();

    let mut fnv_s = 0xcbf29ce484222325u64;
    fnv_mix(&mut fnv_s, s.as_bytes());
    fnv_mix(&mut fnv_s, joined.as_bytes());
    let len_sum = s.len() + joined.len();
    let bytes_sum: u64 = s.bytes().map(|b| b as u64).sum::<u64>()
        + joined.bytes().map(|b| b as u64).sum::<u64>();
    drop(parts);
    drop(joined);
    drop(s);

    let s1 = snap();
    ENABLED.store(false, Relaxed);
    let peak = PEAK_BYTES.swap(0, Relaxed);
    report(
        "string",
        s0,
        s1,
        Some(peak),
        &format!("len_sum={len_sum} bytes_sum={bytes_sum:#x} fnv={fnv_s:#x}"),
    );
}

// ===== phase P4: large alignments plus usable_size anchors =====

#[repr(align(64))]
struct A64(u64, u64);

#[repr(align(4096))]
struct Page([u8; 4096]);

fn phase_align() {
    ENABLED.store(true, Relaxed);
    let s0 = snap();

    let mut av: Vec<A64> = Vec::new();
    let mut l = Lcg(0x243f6a8885a308d3);
    for _ in 0..300 {
        av.push(A64(l.next(), l.next()));
    }
    assert_eq!(av.as_ptr() as usize % 64, 0, "align(64) Vec base address");
    let mut sum_a: u64 = 0;
    for A64(x, y) in &av {
        sum_a = sum_a.wrapping_add(*x).wrapping_add(*y);
    }
    drop(av);

    let mut pages: Vec<Box<Page>> = Vec::new();
    for p in 0..8u8 {
        let mut pg = Box::new(Page([0u8; 4096]));
        assert_eq!(&*pg as *const Page as usize % 4096, 0, "align(4096) Box base address");
        for (i, b) in pg.0.iter_mut().enumerate() {
            *b = p ^ (i as u8);
        }
        pages.push(pg);
    }
    let sum_pages: u64 = pages
        .iter()
        .flat_map(|pg| pg.0.iter())
        .map(|&b| b as u64)
        .sum();
    drop(pages);

    // usable_size granularity anchors (the same vendored mimalloc size-class table, so
    // deterministic across dimensions); stored into a fixed array, no live container across snap
    let mut usable = [0usize; 5];
    for (i, (sz, al)) in [(24usize, 8usize), (1000, 16), (17, 64), (65536, 4096), (262144, 32)]
        .into_iter()
        .enumerate()
    {
        let layout = Layout::from_size_align(sz, al).unwrap();
        let p = unsafe { MiMalloc.alloc(layout) };
        assert!(!p.is_null(), "alloc failed");
        usable[i] = unsafe { MiMalloc.usable_size(p) };
        unsafe { MiMalloc.dealloc(p, layout) };
    }

    let s1 = snap();
    ENABLED.store(false, Relaxed);
    let peak = PEAK_BYTES.swap(0, Relaxed);
    let usable_s = usable
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join("/");
    report(
        "align",
        s0,
        s1,
        Some(peak),
        &format!("sum={sum_a:#x} pages_sum={sum_pages:#x} usable={usable_s}"),
    );
}

// ===== phase P5: cross-thread alloc/free across 4 workers =====

fn worker(w: u64, mut payload: Vec<u8>) -> (u64, u64) {
    // payload is allocated by main and freed in this worker (cross-thread remote free)
    let mut fnv = 0xcbf29ce484222325u64;
    fnv_mix(&mut fnv, &payload);
    payload.clear();
    drop(payload);

    // Fixed-size workload: a mix of small Vec/String/BTreeMap (self-allocated and freed here)
    let mut sum: u64 = w;
    for it in 0..2000u64 {
        let mut v: Vec<u16> = Vec::new();
        for j in 0..(16 + (it % 48)) {
            v.push((it ^ j) as u16);
        }
        sum = sum.wrapping_add(v.iter().map(|&x| x as u64).sum::<u64>());
        let mut st = String::new();
        st.push_str("w");
        st.push_str(&it.to_string());
        sum = sum.wrapping_add(st.len() as u64);
        if it % 500 == 0 {
            let mut bm: BTreeMap<u64, u64> = BTreeMap::new();
            for k in 0..40u64 {
                bm.insert(k ^ (it << 8), k);
            }
            sum = sum.wrapping_add(bm.keys().sum::<u64>());
            sum = sum.wrapping_add(bm.len() as u64);
        }
    }

    // The result payload is allocated here and freed by main (the reverse remote free)
    let mut out: Vec<u8> = Vec::with_capacity(1024);
    let mut l = Lcg(0x100 + w);
    for _ in 0..1024 {
        out.push(l.next() as u8);
    }
    fnv_mix(&mut fnv, &out);
    (fnv, sum.wrapping_add(out.len() as u64))
}

fn phase_threads() {
    ENABLED.store(true, Relaxed);
    let s0 = snap();

    let outs: Vec<(u64, u64)> = std::thread::scope(|sc| {
        let mut handles = Vec::new();
        for w in 0..4u64 {
            let mut payload: Vec<u8> = Vec::with_capacity(2048 + (w as usize) * 512);
            let mut l = Lcg(0x55 + w);
            for _ in 0..payload.capacity() {
                payload.push(l.next() as u8);
            }
            handles.push(sc.spawn(move || worker(w, payload)));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("worker returned normally"))
            .collect()
    });

    let mut fnv_all = 0u64;
    let mut sum_all = 0u64;
    for (f, s) in &outs {
        fnv_all ^= f; // xor is commutative, so the combine is order-independent; join is already ordered
        sum_all = sum_all.wrapping_add(*s);
    }
    drop(outs); // freed in-window so it pairs up (otherwise live_delta counts it)

    let s1 = snap();
    ENABLED.store(false, Relaxed);
    // The peak watermark depends on the scheduling interleaving, so it is not printed for the multi-threaded phase; only interleaving-independent totals are.
    report(
        "threads",
        s0,
        s1,
        None,
        &format!("fnv={fnv_all:#x} sum={sum_all:#x}"),
    );
}

fn main() {
    phase_vec();
    phase_btree();
    phase_string();
    phase_align();
    phase_threads();
    println!("mi_version={}", MiMalloc.version());
    println!(
        "total: calls={} alloc_bytes={}",
        CALLS.load(Relaxed),
        ALLOC_BYTES.load(Relaxed)
    );
    println!("mimalloc_ok");
}
