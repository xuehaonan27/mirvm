#!/usr/bin/env mirvm
---
[dependencies]
# mimalloc 0.1.52（定稿时 0.1.x 最新；拉 libmimalloc-sys 0.1.49 = mimalloc C v3 系，
# 与引擎自身堆后端 heap.rs 同源但异实例：进程内两台 mimalloc，guest 指针全部来自
# guest 侧归档的竞技场）。extended 只为两个确定性锚点：mi_version()（C 库版本常量，
# 证明 vendored C 库确实链入并在跑）与 mi_usable_size()（分配粒度由同一 size-class
# 表决定，跨维逐位确定）。extended 的 stats_json() 不采用：其内容含启动期簿记与
# 运行期竞技场状态，D8k 已证启动簿记分配次数跨维差异（ram-spec §2 unspecified），
# 打印必炸差分。默认 feature（v1 系 API、非 secure、非 override）。
mimalloc = { version = "=0.1.52", features = ["extended"] }
---
// mimalloc 0.1 全局分配器替换边界探针（批7波2 最有意条目）：#[global_allocator]
// = 计数薄包装 CountingMi（委托 MiMalloc）——guest 全部堆分配经 FFI 进 vendored
// C mimalloc 的竞技场（mmap 原生内存，非引擎内建堆），引擎堆模型边界全面受压。
//
// 测试面：
//   P0 锚点  ：mi_version()（C 库版本常量）
//   P1 vec   ：20k push 增长（realloc 路径）+ 64 槽 ×300 轮 vec![0;len] 磨损
//              （alloc_zeroed + 新鲜 alloc + drop 交错）+ 零化语义检查
//   P2 btree ：10_000 键 LCG 序列插入 BTreeMap<u64,u64>，删 key%3==0，遍历求和
//   P3 string：push_str ×1536 片段（String 增长率）+ format! ×256 件 join +
//              shrink_to_fit
//   P4 对齐  ：repr(align(64)) 元素 Vec、repr(align(4096)) 页 Box（对齐断言
//              必过）+ 五组 (size,align) 的 mi_usable_size() 粒度锚点
//   P5 线程  ：std::thread::scope 4 worker：main 侧分配 payload 由 worker 释放
//              （跨线程 remote-free）、worker 侧分配结果经 join 回 main 释放
//              （反向 remote-free）+ 每 worker 定规格分配工作负载；checksum 按
//              worker 序汇合（与调度交织无关的总量/和/xor 才打印，峰值水位只
//              在单线程相位打印）
//   计数层   ：仅在相位窗口内记账（ENABLED 闸）；calls/alloc_bytes/峰值/live
//              增减 全为窗口差值——启动期簿记分配次数跨维差异（D8k，unspecified）
//              被窗口隔离；每相位断言 live_delta==0（全部分配成对释放）。
//   逻辑锚点 ：各相位数据 FNV-1a 与求和（u64 wrapping，跨维逐位确定）。
//
// 确定性：LCG 定种、BTreeMap 序、join 按 worker 序汇合、无时间/地址/HashMap；
// 浮点零使用；stderr 真空（零 warning）。输出 ≈20 行。
//
// 三维复跑命令（仓库根）：
//   A: target/release/mirvm run corpus/c_mimalloc.rs
//   B: cd $(grep -l 'name = "c_mimalloc"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_mimalloc.rs
//
// 【FRONTIER 绕行①：native_archive constructor 卫士（2026-07-17 实测）】
// libmimalloc.a（v3）的 prim.c 带 `__attribute__((constructor)) mi_process_attach`
// （.init_array/.fini_array 各一项，readelf 实锤）；引擎 native_archive 对一切带
// constructor/destructor section 的静态归档响亮拒绝（exit 101，
// "dlopen 生命周期语义尚未定义"，有单测锁定此纪律）。绕行 = CFLAGS 注入
// `-DMI_PRIM_HAS_PROCESS_ATTACH`（prim.c 的 ctor/dtor 被条件编译摘掉，mimalloc
// 首次分配惰性初始化是上游保证的正确路径；两维同一 C 库构建，差分公平无损）。
// 与 snow_noise 的 `--cfg poly1305_force_soft`、ed25519 的 serial-backend env
// 注入同型先例（段内注入 + 头注记录）。因此三维实际命令为：
//   A: CFLAGS="-DMI_PRIM_HAS_PROCESS_ATTACH" target/release/mirvm run corpus/c_mimalloc.rs
//   B: cd $(grep -l 'name = "c_mimalloc"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | head -1 | xargs dirname) && \
//      CFLAGS="-DMI_PRIM_HAS_PROCESS_ATTACH" \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: CFLAGS="-DMI_PRIM_HAS_PROCESS_ATTACH" MIRVM_JIT_THRESHOLD=1 \
//      target/release/mirvm run corpus/c_mimalloc.rs
// gate 接线需走既有 env 注入通道给本 driver 加该 CFLAGS（默认值即拒载，非漏配）。
//
// 【已修复（2026-07-17）｜custom #[global_allocator] 下 `__rust_*` 分配族
//  跨堆撕裂 → 运行期统一路由 shim】
// 修复前本 driver 死于 phase_string（A/C 同点上 SIGSEGV/错崩，B 维 oracle
// 全绿）。两层根因与修法（decision-history §7.7）：
//  ①分配系 builtin 的决定按【lower 会话】做出：base/deps image 在 Default
//    会话把 __rust_* 烘成 CallBuiltin(Rust*)→引擎堆，而 delta/image 的同
//    程序分配走 AST 展开器的 guest shim→用户分配器 ⇒ 两堆互穿 free。
//    修：lower 在 kind=Global 时登记 HIR 展开器生成的四只 shim FuncId
//    （Module.custom_alloc_shims），interp 的 CallBuiltin(Rust*) 臂在运行
//    期统一路由到 shim（分配是程序级语义，与字节码的烘焙会话无关）。
//  ②首次路由实现漏了 rebase：shim 的 FuncId 在 A2 split 收尾未随
//    exports/fn_addrs/sites 同规则映射，运行期 call_guest 打到移位后的
//    野 id，报"ABI 不匹配"错调 insert_entry/from_iter——补 rb.fn_id 后正。
// 保留两枚最小复现备回归（彼时怒态）：/tmp/ga_p_only.rs（System 包装 +
// println 即崩，退出段跨界 free）、/tmp/ga_vecstr.rs（注册表对账报
// CROSS-FREE 6144B）。
//
// <details><summary>原始 EXPECTED-RED 全记录（2026-07-17 定档文本）</summary>
//
// A 维：vec/btree 两相位 stdout 与 native 逐字节一致（窗口内分配计数
// 316/1355 calls 全对）后，死于 phase_string（Vec<String> 增长段）。
// C 维（JIT=1）：同一死点同形态 —— 与 JIT 无关，引擎共享层根因。
// B 维 native：exit 0、stderr 真空、两跑输出逐字节一致（oracle 正常）。
// 实例①最小复现（/tmp/ga_p_only.rs，System 薄包装，全量）：
//     use std::alloc::{GlobalAlloc, Layout, System};
//     struct A;
//     unsafe impl GlobalAlloc for A {
//         unsafe fn alloc(&self, l: Layout) -> *mut u8 { unsafe { System.alloc(l) } }
//         unsafe fn dealloc(&self, p: *mut u8, l: Layout) { unsafe { System.dealloc(p, l) } }
//     }
//     #[global_allocator]
//     static G: A = A;
//     fn main() { println!("hello"); }        // → stdout 正常，退出段 SIGSEGV(139)
// 取证链：LD_PRELOAD SA_SIGINFO si_addr=0x4000/0x8000，崩点符号化 =
// engine::heap::dealloc(heap.rs:23) → mimalloc mi_validate_ptr_page 读野；
// 1024B stdout 缓冲此前经 guest 分配器发出。
// 实例②最小复现核心（/tmp/ga_vecstr.rs；静态指针注册表对账）：
//     for i in 0..256 { v.push(format!(...)) } 后 drop →
//     CROSS-FREE: unknown ptr=0x7f31bc090000 sz=6144 al=8 n=257 + SIGSEGV；
// 6144B 末档缓冲从未经 guest 发出 → 出自引擎 mimalloc；drop 却走 guest 路由。
// 嫌疑落点（umpire 记录）：resolve_call ①/②/③ 支路对 `__rust_*` 的归一、
// engine_builtins 对 Global kind 的注册范围、A2 双 lower 会话对 allocator
// shim 符号的可见性差。—— 两怀疑均被修法证实（①为根因、②为实现自伤）。
//
// </details>
use mimalloc::MiMalloc;
use std::alloc::{GlobalAlloc, Layout};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

// ===== 计数分配器（委托 MiMalloc；窗口内记账）=====

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

// ===== 确定性工具 =====

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
    assert_eq!(live_delta, 0, "{name}: 窗口内有未配对分配");
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

// ===== 相位 P1：Vec 增长 + 磨损 =====

fn phase_vec() {
    ENABLED.store(true, Relaxed);
    let s0 = snap();

    // 增长链（realloc 主路）
    let mut vv: Vec<u64> = Vec::new();
    let mut l = Lcg(0x9e3779b97f4a7c15);
    for _ in 0..20_000 {
        vv.push(l.next());
    }
    let sum_growth: u64 = vv.iter().fold(0u64, |a, &x| a.wrapping_add(x));
    let fnv_growth = fnv_u64s(&vv);
    drop(vv);

    // 磨损环：64 槽 ×300 轮，vec![0;len]（alloc_zeroed）+ 定式回填 + 槽位 drop
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

    // 零化语义：vec![0u16; 512] 必须全零
    let z = vec![0u16; 512];
    assert_eq!(z.iter().map(|&x| x as u64).sum::<u64>(), 0, "zeroed 语义");
    drop(z); // 窗口内配对释放（否则 live_delta 计上）

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

// ===== 相位 P2：BTreeMap 10k 键 =====

fn phase_btree() {
    ENABLED.store(true, Relaxed);
    let s0 = snap();

    let mut m: BTreeMap<u64, u64> = BTreeMap::new();
    let mut l = Lcg(0xdeadbeef12345678);
    for i in 0..10_000u64 {
        let k = l.next().wrapping_add(i << 1);
        m.insert(k, k.rotate_left(17) ^ i);
    }
    assert_eq!(m.len(), 10_000, "插入数");
    let hit = (0..1000u64)
        .filter(|&i| m.contains_key(&i))
        .count() as u64;

    // 删除 key%3==0
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

// ===== 相位 P3：String 拼接 =====

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

// ===== 相位 P4：大对齐 + usable_size 锚点 =====

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
    assert_eq!(av.as_ptr() as usize % 64, 0, "align(64) Vec 基址");
    let mut sum_a: u64 = 0;
    for A64(x, y) in &av {
        sum_a = sum_a.wrapping_add(*x).wrapping_add(*y);
    }
    drop(av);

    let mut pages: Vec<Box<Page>> = Vec::new();
    for p in 0..8u8 {
        let mut pg = Box::new(Page([0u8; 4096]));
        assert_eq!(&*pg as *const Page as usize % 4096, 0, "align(4096) Box 基址");
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

    // usable_size 粒度锚点（同一 vendored mimalloc size-class 表 → 跨维确定）；
    // 直接入定长数组，不留存活容器跨 snap
    let mut usable = [0usize; 5];
    for (i, (sz, al)) in [(24usize, 8usize), (1000, 16), (17, 64), (65536, 4096), (262144, 32)]
        .into_iter()
        .enumerate()
    {
        let layout = Layout::from_size_align(sz, al).unwrap();
        let p = unsafe { MiMalloc.alloc(layout) };
        assert!(!p.is_null(), "alloc 失败");
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

// ===== 相位 P5：4 worker 跨线程分配/释放 =====

fn worker(w: u64, mut payload: Vec<u8>) -> (u64, u64) {
    // payload 由 main 分配、在本 worker 释放（跨线程 remote-free）
    let mut fnv = 0xcbf29ce484222325u64;
    fnv_mix(&mut fnv, &payload);
    payload.clear();
    drop(payload);

    // 定规格工作负载：小 Vec/String/BTreeMap 混合（跨线程自身分配释放）
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

    // 结果 payload 在本 worker 分配，回 main 释放（反向 remote-free）
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
            .map(|h| h.join().expect("worker 正常返回"))
            .collect()
    });

    let mut fnv_all = 0u64;
    let mut sum_all = 0u64;
    for (f, s) in &outs {
        fnv_all ^= f; // xor 满足交换律（汇聚与序无关）；此处 join 本已按序
        sum_all = sum_all.wrapping_add(*s);
    }
    drop(outs); // 窗口内配对释放（否则 live_delta 计上）

    let s1 = snap();
    ENABLED.store(false, Relaxed);
    // 峰值水位受调度交织影响，多线程相位不打印；仅总量（与交织无关）。
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
