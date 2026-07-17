#!/usr/bin/env mirvm
---
[dependencies]
# zstd 钉 exact 0.13.3（绑定 C zstd 1.5.7 / zstd-sys 2.0.16 / zstd-safe 7.2.4，
# 全在本地 registry 缓存）。默认特性（legacy/arrays/zdict_builder）保留；
# 补开非默认的 zstdmt 且仅为它：zstd-sys 只在 zstdmt 下给 C 库编
# ZSTD_MULTITHREAD + -pthread，否则 NbWorkers(2) 只回确定性错误串
# "unsupported parameter"，达不到本 driver 的线程面测试面（zstd::safe::CCtx
# set_parameter 不经任何 feature 门控，受门控的是 C 库是否带多线程代码）。
zstd = { version = "=0.13.3", features = ["zstdmt"] }
---
// zstd 0.13.3 长输入三维差分：16MiB 程序生成的确定性三层混合数据 →
// level 1/9 全量压缩、level 19 只压前 4MiB 切片收口成本、外加一次
// zstd::safe::CCtx 多线程参数路径（CompressionLevel(3) + NbWorkers(2)，
// 一次性 compress2；C 层实际走 ZSTDMT 分 job 并行，帧字节与同参数各次运行
// 逐字节可复现——线程调度不进输出）。压缩/解压计算主体在 native C 里跑，
// 两个维度同源同参，帧字节天然逐字节一致（c_zstd_stream 已实证该通道）。
//
// 数据生成（全部仅依赖定种 LCG，无时间/rand/env）：
//   text  8MiB = 2MiB 定种 LCG 拼装的日志体 unique 文本 ×4（extend_from_within
//          翻倍；人为制造远程重复，供高档大窗口/LDM 命中）；
//   rep   4MiB = 周期 [6,127,4096,250000] 四段各 1MiB 纯周期字节段
//          （段内严格 p-周期：翻倍保持 p 整倍长，尾部单段补齐同因）；
//   rnd   4MiB = 定种 LCG 小端字节流（近不可压段）。
// 结构统计：每层长度/生成周期 + 熵特征计数（在层首 256KiB 采样窗上数
// 不同字节值数/相邻等值连跑次数/零字节数/最长等值连跑），全确定性整数。
// 指纹：fnv64 = 按 8 字节小端块滚动的 FNV-1a 变体（尾部零填充成块），
// 逐字节大输入下保持解释器可承受的迭代量；与 FNV-1a 同种子同素数。
//
// 覆盖清单：
//   ① level 1 / 9 全量 16MiB bulk::compress → 尺寸 + fnv64 + 千分位整数比
//      + bulk::decompress 逐字节 roundtrip 断言；
//   ② level 19 仅前 4MiB 切片（收口 btopt 成本）同口径；
//   ③ zstd::safe::CCtx 参数路径：CompressionLevel(3) + NbWorkers(2) 一次性
//      compress2（ZSTDMT 真并行面），同口径 roundtrip；
//   ④ 常量锚点：min/max/default level + runtime version_number()。
//
// 确定性：只打印整数/布尔/hex 指纹；无绝对路径、地址、HashMap 序；
// 无 warning；stderr 真空。roundtrip 比对 = Vec<u8> ==（memcmp 逐字节）。
//
// 成本声明：B 维 level 19 压 4MiB 高冗余文本为秒级到十几秒级（btopt），
// level 9 全量为亚秒到秒级；A/C 维压缩主体同在 native C，解释器/JIT 只承担
// 数据生成与指纹（数百万次小迭代）。预计各维远低于 60s。
//
// 三维复跑（A 首跑后 B 才有脚本目录）：
//   A: target/release/mirvm run corpus/c_zstd_long.rs
//   B: cd "$(dirname "$(grep -l 'name = "c_zstd_long"' ~/.cache/mirvm/scripts/*/Cargo.toml)")" && \
//      RUSTC="$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc" \
//      "$HOME/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo" run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_zstd_long.rs
//
// FRONTIER 绕行：无（多线程帧在生成本机即可逐字节复现）。
use zstd::zstd_safe::{CCtx, CParameter};

const MIB: usize = 1024 * 1024;
const TEXT_UNIQUE: usize = 2 * MIB;
const TEXT_LAYER: usize = 8 * MIB;
const REPEAT_SEG: usize = MIB;
const REPEAT_PERIODS: [usize; 4] = [6, 127, 4096, 250_000];
const RANDOM_LAYER: usize = 4 * MIB;
const STAT_WINDOW: usize = 256 * 1024;

/// 定种 LCG（MMIX 参数；wrapping u64，跨平台同序列）。
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
}

/// 按 8 字节块滚动的 FNV-1a 变体指纹（迭代量 1/8，确定性同 FNV-1a）。
fn fnv64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut it = data.chunks_exact(8);
    for c in &mut it {
        h ^= u64::from_le_bytes(c.try_into().unwrap());
        h = h.wrapping_mul(0x100000001b3);
    }
    let rem = it.remainder();
    let mut tail = [0u8; 8];
    tail[..rem.len()].copy_from_slice(rem);
    h ^= u64::from_le_bytes(tail);
    h = h.wrapping_mul(0x100000001b3);
    h
}

fn push_dec(v: &mut Vec<u8>, mut x: u64) {
    let mut buf = [0u8; 20];
    let mut n = 0;
    loop {
        buf[n] = b'0' + (x % 10) as u8;
        x /= 10;
        n += 1;
        if x == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        v.push(buf[n]);
    }
}

const TEXT_TOKENS: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
    "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
    "sierra", "tango", "uniform", "victor", "whiskey", "xray",
];

/// text 层的 2MiB unique 文本：`>L<行号> <3-5 词> crc=<0..999>\n` ×至满。
fn gen_unique_text(target: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(target + 128);
    let mut t = Lcg(0x9E3779B97F4A7C15);
    let mut n: u64 = 0;
    while v.len() < target {
        v.extend_from_slice(b">L");
        push_dec(&mut v, n);
        let words = 3 + ((t.next() >> 44) % 3) as usize;
        for _ in 0..words {
            v.push(b' ');
            v.extend_from_slice(TEXT_TOKENS[((t.next() >> 33) % 24) as usize].as_bytes());
        }
        v.extend_from_slice(b" crc=");
        push_dec(&mut v, (t.next() >> 11) % 1000);
        v.push(b'\n');
        n += 1;
    }
    v.truncate(target);
    v
}

/// rep 层：每个周期段 = p 字节定种小写模式翻倍到 ≤seg，尾部单段补齐
/// （长保持 p 整倍 → 内容严格 p-周期）。
fn gen_repeat_layer(seg: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(REPEAT_PERIODS.len() * seg + 8);
    let mut t = Lcg(0x1B2C3D4E5F607182);
    for &p in &REPEAT_PERIODS {
        let mut pat = Vec::with_capacity(p);
        for _ in 0..p {
            pat.push(b'a' + ((t.next() >> 29) % 26) as u8);
        }
        let mut block = pat.clone();
        while block.len() * 2 <= seg {
            block.extend_from_within(0..);
        }
        if block.len() < seg {
            let need = seg - block.len();
            block.extend_from_within(0..need);
        }
        out.extend_from_slice(&block);
    }
    out
}

/// rnd 层：定种 LCG 小端字节流。
fn gen_random_layer(target: usize) -> Vec<u8> {
    let mut t = Lcg(0xDEADF00D12345678);
    let mut out = Vec::with_capacity(target + 8);
    while out.len() < target {
        out.extend_from_slice(&t.next().to_le_bytes());
    }
    out.truncate(target);
    out
}

/// 层首 256KiB 采样窗上的熵特征计数（全确定性）。
fn print_stats(label: &str, d: &[u8]) {
    let n = d.len().min(STAT_WINDOW);
    let s = &d[..n];
    let mut seen = [false; 256];
    let mut runs = 0usize;
    let mut zeros = 0usize;
    let mut maxrun = 0usize;
    let mut cur = 0usize;
    let mut prev = 0u8;
    for i in 0..n {
        let b = s[i];
        seen[b as usize] = true;
        if b == 0 {
            zeros += 1;
        }
        if i > 0 && b == prev {
            runs += 1;
            cur += 1;
        } else {
            cur = 1;
        }
        if cur > maxrun {
            maxrun = cur;
        }
        prev = b;
    }
    let distinct = seen.iter().filter(|&&f| f).count();
    println!(
        "stats {label} win={n} distinct={distinct} runs={runs} zeros={zeros} maxrun={maxrun} fnv64={:016x}",
        fnv64(d)
    );
}

/// 单档压缩 + 逐字节 roundtrip + 千分位比 + 指纹，一行。
fn roundtrip_line(label: &str, level: i32, input: &[u8]) {
    let comp = zstd::bulk::compress(input, level).unwrap();
    let back = zstd::bulk::decompress(&comp, input.len()).unwrap();
    let rt = back == input;
    assert!(rt, "{label} roundtrip mismatch");
    println!(
        "one {label} level={level} raw={} comp={} permille={} cfnv64={:016x} rt={rt}",
        input.len(),
        comp.len(),
        comp.len() as u64 * 1000 / input.len() as u64,
        fnv64(&comp)
    );
}

fn main() {
    // ---- 数据：text(8MiB) rep(4MiB) rnd(4MiB) 三层拼接，共 16MiB ----
    let text_u = gen_unique_text(TEXT_UNIQUE);
    let mut text = text_u.clone();
    while text.len() < TEXT_LAYER {
        text.extend_from_within(0..);
    }
    let rep = gen_repeat_layer(REPEAT_SEG);
    let rnd = gen_random_layer(RANDOM_LAYER);

    println!(
        "layout text={} rep={} rnd={} total={}",
        text.len(),
        rep.len(),
        rnd.len(),
        text.len() + rep.len() + rnd.len()
    );
    println!(
        "periods text-unique={} rep={:?} rnd=none",
        TEXT_UNIQUE, REPEAT_PERIODS
    );
    print_stats("text", &text);
    print_stats("rep ", &rep);
    print_stats("rnd ", &rnd);

    let mut data = text;
    data.extend_from_slice(&rep);
    data.extend_from_slice(&rnd);
    println!("raw len={} fnv64={:016x}", data.len(), fnv64(&data));

    // ---- ① level 1 / 9 全量；② level 19 仅前 4MiB 切片 ----
    roundtrip_line("l1 ", 1, &data);
    roundtrip_line("l9 ", 9, &data);
    roundtrip_line("l19", 19, &data[..4 * MIB]);

    // ---- ③ zstd::safe::CCtx 多线程参数路径（level 3 + NbWorkers=2）----
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(3)).unwrap();
    cctx.set_parameter(CParameter::NbWorkers(2)).unwrap();
    let mut mt: Vec<u8> = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
    let n = cctx.compress2(&mut mt, &data).unwrap();
    assert_eq!(n, mt.len(), "mt written len mismatch");
    let back = zstd::bulk::decompress(&mt, data.len()).unwrap();
    let rt = back == data;
    assert!(rt, "mt roundtrip mismatch");
    println!(
        "mt level=3 nbw=2 raw={} comp={} permille={} cfnv64={:016x} rt={rt}",
        data.len(),
        mt.len(),
        mt.len() as u64 * 1000 / data.len() as u64,
        fnv64(&mt)
    );

    // ---- ④ 常量锚点 ----
    println!(
        "levels min={} max={} default={} ver={}",
        zstd::zstd_safe::min_c_level(),
        zstd::zstd_safe::max_c_level(),
        zstd::DEFAULT_COMPRESSION_LEVEL,
        zstd::zstd_safe::version_number()
    );
}
