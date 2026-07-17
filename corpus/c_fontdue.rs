#!/usr/bin/env mirvm
---
[dependencies]
# 钉 =0.9.3：crates.io sparse index 当日查询 fontdue 现行最新稳定即 0.9.3。
# 绕行记录（官方后端开关，非裁剪）：default-features=false 关掉默认 simd
# feature → fontdue 走自带 simd_core 纯 f32 标量后备（fontdue 官方等价实现，
# 其内部测试逐项断言与 f32 基础运算一致；两维用同一 feature 面，原生侧同样
# 标量）。simd 路径在 x86_64 走 core::arch 的 _mm_add_ps/_mm_sqrt_ps/_mm_div_ps/
# _mm_cvttps_epi32 等 14 个 SSE intrinsic（见 fontdue-0.9.3/src/platform/
# simd_x86.rs），与 c_resvg 头注记录的 tiny-skia simd TRAP（mirvm 未内建
# llvm.x86.sse 族外部符号）同族。可选 hashbrown/rayon 保持关闭（parallel
# feature 引入线程调度序，且 cache 实现差异不影响输出文本）。
fontdue = { version = "=0.9.3", default-features = false }
---
// fontdue 0.9.3 字体光栅差分（标量后端，见上钉版注）。
// 覆盖清单：
// ① 字体装载：固定绝对路径 /usr/share/fonts/truetype/dejavu/DejaVuSans.ttf
//    （本机 2016 版 DejaVuSans），文件缺席 → eprintln 固定文本 + exit(2)，
//    不做条件跳过；解析失败 → exit(3)。读入字节打 len + FNV-1a 锁两侧同文件。
// ② 字体级探针：name（Name ID 4）、units_per_em bits、glyph_count、
//    chars 映射表条目数（只取 len，绝不迭代 HashMap）、scale_factor bits、
//    水平/垂直 line metrics @24px 四分量 bits（DejaVu 无垂直 metrics → none）。
// ③ 覆盖探测（确定性选择，非跳过）：CJK 三字（U+4E2D/6587/5B57，DejaVu
//    不含 → probe miss 行）与连字 U+FB00..FB03（ﬀ/ﬁ/ﬂ/ﬃ，命中则纳入
//    光栅集）——规格主串 = ASCII + Latin-1 Supplement + Latin Extended-A，
//    字体覆盖到的探测段字符自动加入 rasterize 面。
// ④ 主光栅：固定串 × 8 档尺寸 [6,9,12,15,18,24,36,60]px（fontdue 尺寸
//    参数即 px），每 glyph 一行：gid、metrics 整型 xmin/ymin/width/height、
//    advance_width/height 位型、OutlineBounds 四 f32 位型、bitmap 长度与
//    FNV-1a。空格等零面积 glyph 照常打（bitmap len=0 边界）。
// ⑤ kern：水平 kern 三对（A-V / T-o / f-i）@24px，None→none，Some→bits，
//    覆盖 ttf-parser opentype-layout GPOS/kern 表路径。
// ⑥ 合计行：全部 glyph 行文本逐字节滚入聚合 FNV-1a 一行收尾。
// 确定性：全部固定常量；f32 一律 to_bits；循环序 = 两个显式数组序；无随机/
// 时间/HashMap 迭代/地址；正常路径 stderr 为空。
use fontdue::{Font, FontSettings};

const FONT_PATH: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
const SIZES: [f32; 8] = [6.0, 9.0, 12.0, 15.0, 18.0, 24.0, 36.0, 60.0];

// 主串：ASCII（含空格/标点/数字零面积与轮廓混合面）
const ASCII_RUN: &str = "Hello, World! 0123456789";
// Latin-1 Supplement + Latin Extended-A
const LATIN_RUN: &str = "ÀáÂäÇçÉèÑñÿĀāČčŒœŠšŽž";
// 探测段：CJK（预计 miss，probe 行为锁定证据）与连字对（命中则光栅）
const CJK_PROBE: &str = "中文字";
const LIG_PROBE: &str = "\u{FB00}\u{FB01}\u{FB02}\u{FB03}";

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let bytes = match std::fs::read(FONT_PATH) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("FATAL: cannot read {FONT_PATH}: {e}");
            std::process::exit(2);
        }
    };
    println!(
        "file path='{}' len={} fnv={:016x}",
        FONT_PATH,
        bytes.len(),
        fnv1a(&bytes)
    );
    let font = match Font::from_bytes(bytes, FontSettings::default()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("FATAL: cannot parse {FONT_PATH}: {e}");
            std::process::exit(3);
        }
    };

    // ---- 字体级探针 ----
    println!("font name={:?}", font.name());
    println!(
        "font upem_bits={:08x} glyphs={} charmap={} sf24_bits={:08x}",
        font.units_per_em().to_bits(),
        font.glyph_count(),
        font.chars().len(),
        font.scale_factor(24.0).to_bits()
    );
    match font.horizontal_line_metrics(24.0) {
        Some(lm) => println!(
            "lm24 asc={:08x} desc={:08x} gap={:08x} nls={:08x}",
            lm.ascent.to_bits(),
            lm.descent.to_bits(),
            lm.line_gap.to_bits(),
            lm.new_line_size.to_bits()
        ),
        None => println!("lm24 none"),
    }
    match font.vertical_line_metrics(24.0) {
        Some(lm) => println!(
            "vlm24 asc={:08x} desc={:08x} gap={:08x} nls={:08x}",
            lm.ascent.to_bits(),
            lm.descent.to_bits(),
            lm.line_gap.to_bits(),
            lm.new_line_size.to_bits()
        ),
        None => println!("vlm24 none"),
    }

    // ---- kern 路径 ----
    for (l, r) in [('A', 'V'), ('T', 'o'), ('f', 'i')] {
        match font.horizontal_kern(l, r, 24.0) {
            Some(k) => println!("kern U+{:04X} U+{:04X} bits={:08x}", l as u32, r as u32, k.to_bits()),
            None => println!("kern U+{:04X} U+{:04X} none", l as u32, r as u32),
        }
    }

    // ---- 覆盖探测 + 光栅集装配（数组序显式固定）----
    let mut run: Vec<char> = ASCII_RUN.chars().chain(LATIN_RUN.chars()).collect();
    for c in CJK_PROBE.chars() {
        let gid = font.lookup_glyph_index(c);
        if gid == 0 {
            println!("probe U+{:04X} gid=0 miss", c as u32);
        } else {
            println!("probe U+{:04X} gid={} hit", c as u32, gid);
            run.push(c);
        }
    }
    for c in LIG_PROBE.chars() {
        let gid = font.lookup_glyph_index(c);
        if gid == 0 {
            println!("probe U+{:04X} gid=0 miss", c as u32);
        } else {
            println!("probe U+{:04X} gid={} hit", c as u32, gid);
            run.push(c);
        }
    }
    println!("rune count={}", run.len());

    // ---- 主光栅循环：8 档 × 全 rune ----
    let mut agg: u64 = 0xcbf29ce484222325;
    let mut n: u64 = 0;
    for &px in &SIZES {
        for &c in &run {
            let gid = font.lookup_glyph_index(c);
            let (m, bmp) = font.rasterize(c, px);
            let bh = fnv1a(&bmp);
            let line = format!(
                "p{:02} U+{:04X} gid={} x={} y={} w={} h={} aw={:08x} ah={:08x} bx={:08x} by={:08x} bw={:08x} bh={:08x} len={} fnv={:016x}",
                px as u32,
                c as u32,
                gid,
                m.xmin,
                m.ymin,
                m.width,
                m.height,
                m.advance_width.to_bits(),
                m.advance_height.to_bits(),
                m.bounds.xmin.to_bits(),
                m.bounds.ymin.to_bits(),
                m.bounds.width.to_bits(),
                m.bounds.height.to_bits(),
                bmp.len(),
                bh
            );
            for &b in line.as_bytes() {
                agg ^= b as u64;
                agg = agg.wrapping_mul(0x100000001b3);
            }
            n += 1;
            println!("{line}");
        }
    }
    println!("total glyphs={} fnv={:016x}", n, agg);
}
