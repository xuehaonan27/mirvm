#!/usr/bin/env mirvm
---
[dependencies]
zune-jpeg = "0.4"
image = { version = "0.25", default-features = false, features = ["jpeg"] }
jpeg-encoder = "0.7"
---
// zune-jpeg 0.4（0.4.21，0.4.x 末版；规格钉 0.4 谱系，crates.io 已有 0.5.x 但不入
// 本案）纯 Rust JPEG 解码三维差分。
//
// 覆盖：
// ① 两幅程序化 RGB 图：G=64x48 平滑渐变（r/g/b 各为 x,y 的整数线性/二次组合，
//    低 AC 能量）；S=73x41 哨兵图（odd 宽高：73%16≠0、41%8≠0 → 强迫 MCU 右/下
//    边界 padding 与 upsample 边缘路径；8px 棋盘 + xor 高频纹理 + 四角/末行/末列
//    纯色哨兵，高 AC 能量喂饱 progressive 多 scan 的细化路径）。
// ② 五帧编码流（逐字节 fnv 锚定编码器输出）：
//      G-base    image::codecs::jpeg::JpegEncoder q=92（image 官方：JpegEncoder
//                恒为 4:2:2 子采样、Baseline-DCT）；
//      S-base    image JpegEncoder q=80（同为 4:2:2 baseline）；
//      G-prog    jpeg-encoder 0.7.0 q=88 + set_progressive(true) +
//                set_progressive_scans(4) + set_sampling_factor(F_2_2/4:2:0) +
//                set_restart_interval(99)：DRI 段真实存在（dri=99）但 4:2:0 下
//                每 scan 仅 12 MCU < 99 → 无重启标记触发（理由见 ⑦ 的上游破洞）；
//      S-prog    jpeg-encoder 0.7.0 q=95 + set_progressive(true) +
//                set_progressive_scans(5) + set_sampling_factor(F_1_1/4:4:4)，RI=0；
//      G-seq-ri  jpeg-encoder 0.7.0 顺序模式 q=88 + F_2_2 + RI=5：顺序扫描内
//                重启标记真实触发且 zune-jpeg 0.4.21 正常解码（与其余两条解码
//                线逐位一致），补齐"重启路径真实走到"这一面。
//    三种 chroma 布局（4:2:2/4:2:0/4:4:4）→ 解码器 upsample 矩阵全分支。
// ③ 每帧 zune-jpeg 0.4.21 解码：decode_headers → info()（宽高/components）、
//    get_input/output_colorspace、sof Debug 文本、sof.is_progressive()；显式
//    jpeg_set_out_colorspace(RGB) 后 decode() 全帧像素。
// ④ decode info 自解析 restart interval：zune-jpeg 0.4.21 的 restart_interval
//    字段为 pub(crate) 无公开 getter，故 driver 内做 JPEG 段走查（SOI→各头段→
//    SOS 前遇 DRI/FFDD 取值），无熵数据误报（stuffed 0xFF00、RSTn=FFD0-D7 均
//    ≠DD，DRI 仅存于扫描前头段）。base 帧=0（image 编码器不写 DRI）、G-prog=99、
//    S-prog=0、G-seq-ri=5——prog 标志 + dri 两值确认两条编码路径都真正走到。
// ⑤ 与 image 0.25.10 解码结果逐像素比对（load_from_memory_with_format→to_rgb8）。
//    image 的 jpeg 后端自身即 zune-jpeg 0.5.x（其 Cargo.toml [dependencies]
//    .zune-jpeg version="0.5.5" caret 解析 0.5.x 最新，无法摘其 default
//    features）→ 构成 0.4.21 与 0.5.x 两条解码代码线的跨版本对拍；
//    mismatch/first 计数逐帧打印。实测语义：4:2:2 帧两版逐位一致（mism=0）；
//    4:2:0 帧（G-prog/G-seq-ri）两版差 48 像素（mism=48 first=62，全在垂直
//    色度 upsample 含区域的舍入差——T.81 未约束 IDCT/上采样逐位精确，同帧在
//    各解码版本自身下 progressive 与 sequential 复现逐位相同 → 差异确在
//    0.4↔0.5 解码线，不在编码路径）；4:4:4 的 S-prog 又归零。该计数本身是
//    确定值，三维逐字节一致。
// ⑥ 每帧 FNV-1a（zune 解码 RGB 全帧）+ 抽样像素（(0,0)/(w/3,h/2)/(w-1,h-1)）
//    的 #rrggbb 十六进制 bits；帧源图原始字节 fnv 亦锚定。
// ⑦ 上游破洞探针（如实记录、不进主干）：zune-jpeg 0.4.21 无法解码"progressive
//    扫描 + 重启标记真实触发"的流。探针矩阵（native，jpeg-encoder 产出同图）：
//      seq+RI6    → z04 OK / z05 OK / image OK
//      prog+RI0   → 全 OK
//      prog+RI2   → z04 ERR "Error in decoding MCU. Reason Marker SOS found in
//                   bitstream, possibly corrupt jpeg" / z05 OK / image OK
//      prog+RI99  → 全 OK（enc_len 恰比 RI0 多 6 字节=DRI 段本体 → 零重启标记）
//    流的合法性由 zune-jpeg 0.5.x 与 image 均能正常解码证实 → 破洞在 zune 0.4.21
//    的 progressive 重启处理（0.5 已修）。⑦ 段以确定性错误文本如实打印该洞
//    （错误文本两维也逐字节一致），并以 image 解码 fnv 佐证流有效；G-prog 的
//    RI=99 即为绕开该洞又让 dri 非零进 decode info 的选值。
//
// 确定性：全内存操作、全整数数学；无文件/壁钟/真随机/线程；无 HashMap 迭代；
// 无浮点（无一处 to_string(f64)）；错误文本为库内静态字符串；stderr 真空。
//
// 绕行/钉版本记录：
// ⚠ 第三依赖 jpeg-encoder = "0.7"（0.7.0）为规格落地必需：任务书要求"用 image
//   编码 … 各为 baseline 与 progressive"，但 image 0.25 的 JpegEncoder 官方仅
//   实现 Baseline 标准（docs.rs image 0.25.9 codecs::jpeg 模块页原文 "This
//   module implements the Baseline JPEG standard."，JpegEncoder 方法集仅
//   new/new_with_quality/set_pixel_density/encode*，无 progressive 面）。故
//   progressive 流由 jpeg-encoder 产出（set_progressive/set_progressive_scans/
//   set_restart_interval 官方公开开关），image 仍承担 baseline 编码与参照解码
//   两个规格主干面。jpeg-encoder default=["std"]（"simd" feature 非默认 →
//   编码全标量路径，不存在 AVX2 编码分支）。
// ⚠ zune-jpeg = "0.4" 保留 default features（x86/neon/std）：x86 下保留运行期
//   is_x86_feature_detected 派发 SSE/AVX 解码快径；三维同机同特征集，mirvm 侧
//   有 guest CPUID 派发 + x86 helper 面（src/vm/engine/x86.rs），若撞未内建
//   intrinsic 即如实定红 ①，不预先摘 feature 回避探测面。
// ⚠ image = "0.25" 用 default-features=false + ["jpeg"]：本 driver 只用 jpeg
//   编/解码面，摘掉 png/gif 等无关特性收敛依赖树（行为面不受影响）。
// ⚠ G-prog 选 RI=99、另立 G-seq-ri 帧：绕开 ⑦ 的 zune 0.4.21 上游破洞（属
//   "避上游破洞"类，非裁剪规格主干——progressive/baseline 双路径解码与像素
//   对拍全部保留，破洞本身以确定性文本如实打印）。
// 版本解析（2026-07-17，脚本工程 Cargo.lock 实测）：zune-jpeg 0.4.21 /
// image 0.25.10 / jpeg-encoder 0.7.0；zune-core 0.4.12（随 0.4.21）；image
// 的传递解码后端 zune-jpeg 0.5.15 / zune-core 0.5.1。

use image::ExtendedColorType;
use image::ImageFormat;
use image::codecs::jpeg::JpegEncoder as ImgJpegEncoder;
use jpeg_encoder::{ColorType as JeColor, Encoder as JeEncoder, SamplingFactor};
use zune_jpeg::JpegDecoder as ZuneDecoder;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const GW: u32 = 64;
const GH: u32 = 48;
const SW: u32 = 73;
const SH: u32 = 41;

/// G 图：64x48 平滑三通道渐变（低 AC 能量）。
fn build_gradient() -> Vec<u8> {
    let mut v = Vec::with_capacity((GW * GH * 3) as usize);
    for y in 0..GH {
        for x in 0..GW {
            let r = (x * 255 / (GW - 1) + y * 64 / (GH - 1)) & 0xff;
            let g = (y * 255 / (GH - 1)) & 0xff;
            let b = ((x * x + y * 3) % 192) & 0xff;
            v.push(r as u8);
            v.push(g as u8);
            v.push(b as u8);
        }
    }
    v
}

/// S 图：73x41 哨兵 odd 尺寸，8px 棋盘 + xor 高频纹理 + 边界哨兵。
fn build_sentinel() -> Vec<u8> {
    let mut v = Vec::with_capacity((SW * SH * 3) as usize);
    for y in 0..SH {
        for x in 0..SW {
            if x == 0 && y == 0 {
                v.extend_from_slice(&[0x00, 0x00, 0x00]); // 左上黑哨兵
            } else if x == SW - 1 && y == 0 {
                v.extend_from_slice(&[0xff, 0xff, 0xff]); // 右上白哨兵
            } else if x == 0 && y == SH - 1 {
                v.extend_from_slice(&[0xff, 0x00, 0x00]); // 左下红哨兵
            } else if x == SW - 1 && y == SH - 1 {
                v.extend_from_slice(&[0x00, 0x00, 0xff]); // 右下蓝哨兵
            } else if x == SW - 1 {
                v.extend_from_slice(&[0x00, 0xff, 0x00]); // 末列纯绿哨兵
            } else if y == SH - 1 {
                v.extend_from_slice(&[0xff, 0xff, 0x00]); // 末行纯黄哨兵
            } else {
                let tile = ((x / 8) + (y / 8)) % 2;
                let n = ((x * 37) ^ (y * 91) ^ (x.wrapping_mul(y))) & 0x3f;
                let base: u32 = tile * 0xc0;
                v.push((base ^ n) as u8);
                v.push((base.wrapping_add(0x20) ^ (n << 1)) as u8);
                v.push((0xff - base ^ (n >> 1)) as u8);
            }
        }
    }
    v
}

/// baseline 路径：image 0.25 JpegEncoder（官方恒 4:2:2、Baseline-DCT）。
fn enc_baseline(data: &[u8], w: u32, h: u32, quality: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut enc = ImgJpegEncoder::new_with_quality(&mut buf, quality);
    enc.encode(data, w, h, ExtendedColorType::Rgb8).unwrap();
    buf
}

/// jpeg-encoder 0.7 通用编码（顺序/progressive、子采样、重启间隔全可控）。
/// 规格主干里 progressive 帧必须走这条路：image 无 progressive 编码面（头注 ⚠）。
fn enc_je(data: &[u8], w: u32, h: u32, quality: u8, progressive: bool, scans: u8, ri: u16, sf: SamplingFactor) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut enc = JeEncoder::new(&mut buf, quality);
    enc.set_progressive(progressive);
    if progressive {
        enc.set_progressive_scans(scans);
    }
    enc.set_restart_interval(ri);
    enc.set_sampling_factor(sf);
    enc.encode(data, w as u16, h as u16, JeColor::Rgb).unwrap();
    buf
}

/// JPEG 段走查取 DRI（zune-jpeg 0.4.21 无公开 getter，见头注 ④）。
/// 返回 0 = 无 DRI 段；u16::MAX / MAX-1 / MAX-2 = 意外格式（不应发生）。
fn restart_interval(b: &[u8]) -> u16 {
    if b.len() < 4 || b[0] != 0xff || b[1] != 0xd8 {
        return u16::MAX;
    }
    let mut p = 2usize;
    loop {
        if p >= b.len() {
            return 0;
        }
        if b[p] != 0xff {
            return u16::MAX - 1;
        }
        while p < b.len() && b[p] == 0xff {
            p += 1; // 填充字节
        }
        if p >= b.len() {
            return 0;
        }
        let m = b[p];
        p += 1;
        if m == 0x00 || (0xd0..=0xd9).contains(&m) {
            continue; // 独立标记（stuffed 0/SOI/RSTn/EOI）
        }
        if m == 0xda {
            return 0; // SOS：熵数据开始，DRI 只合法于扫描前
        }
        if p + 2 > b.len() {
            return 0;
        }
        let seg_len = u16::from_be_bytes([b[p], b[p + 1]]) as usize;
        if m == 0xdd && seg_len == 4 && p + 4 <= b.len() {
            return u16::from_be_bytes([b[p + 2], b[p + 3]]);
        }
        if seg_len < 2 {
            return u16::MAX - 2;
        }
        p += seg_len;
    }
}

/// 打印一帧完整画像：zune 解码 + info + image 对拍。
fn run_frame(tag: &str, enc: &[u8]) {
    println!("{}: enc len={} fnv={:016x}", tag, enc.len(), fnv1a(enc));
    let dri = restart_interval(enc);

    // —— zune-jpeg 0.4.21 解码面 ——
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut dec = ZuneDecoder::new_with_options(&enc[..], opts);
    dec.decode_headers().unwrap();
    let info = dec.info().unwrap();
    let (zw, zh) = dec.dimensions().unwrap();
    println!(
        "{}: info {}x{} comp={} in_cs={:?} out_cs={:?} sof={:?} prog={} dri={}",
        tag,
        zw,
        zh,
        info.components,
        dec.get_input_colorspace().unwrap(),
        dec.get_output_colorspace().unwrap(),
        info.sof,
        info.sof.is_progressive(),
        dri
    );
    let zpix = dec.decode().unwrap();
    println!("{}: zune len={} fnv={:016x}", tag, zpix.len(), fnv1a(&zpix));

    // 抽样像素十六进制 bits
    let (w, h) = (zw as usize, zh as usize);
    let coords = [(0usize, 0usize), (w / 3, h / 2), (w - 1, h - 1)];
    let mut line = String::new();
    for (x, y) in coords {
        let o = (y * w + x) * 3;
        line.push_str(&format!(
            " ({},{})#{:02x}{:02x}{:02x}",
            x, y, zpix[o], zpix[o + 1], zpix[o + 2]
        ));
    }
    println!("{}: samples{}", tag, line);

    // —— image 0.25.10 参照解码（后端 zune-jpeg 0.5.x → 跨版本对拍）——
    let dimg = image::load_from_memory_with_format(&enc[..], ImageFormat::Jpeg).unwrap();
    let (iw, ih) = (dimg.width(), dimg.height());
    let ipix = dimg.to_rgb8().into_raw();
    let mut mism = 0usize;
    let mut first: i64 = -1;
    if zpix.len() == ipix.len() {
        for (i, (a, b)) in zpix.chunks_exact(3).zip(ipix.chunks_exact(3)).enumerate() {
            if a != b {
                mism += 1;
                if first < 0 {
                    first = i as i64;
                }
            }
        }
    } else {
        mism = usize::MAX;
        first = -2;
    }
    println!(
        "{}: xcmp image {}x{} ipsfnv={:016x} mism={} first={}",
        tag,
        iw,
        ih,
        fnv1a(&ipix),
        mism,
        first
    );
}

fn main() {
    let g = build_gradient();
    let s = build_sentinel();
    println!(
        "src: G {}x{} fnv={:016x} S {}x{} fnv={:016x}",
        GW,
        GH,
        fnv1a(&g),
        SW,
        SH,
        fnv1a(&s)
    );

    // 主干五帧
    run_frame("G-base   ", &enc_baseline(&g, GW, GH, 92));
    run_frame("S-base   ", &enc_baseline(&s, SW, SH, 80));
    run_frame("G-prog   ", &enc_je(&g, GW, GH, 88, true, 4, 99, SamplingFactor::F_2_2));
    run_frame("S-prog   ", &enc_je(&s, SW, SH, 95, true, 5, 0, SamplingFactor::F_1_1));
    run_frame("G-seq-ri ", &enc_je(&g, GW, GH, 88, false, 0, 5, SamplingFactor::F_2_2));

    // —— ⑦ 上游破洞探针：zune 0.4.21 × progressive+重启真实触发 ——
    {
        let hole = enc_je(&g, GW, GH, 88, true, 4, 6, SamplingFactor::F_2_2);
        println!("hole: enc len={} fnv={:016x} dri={}", hole.len(), fnv1a(&hole), restart_interval(&hole));
        let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut dec = ZuneDecoder::new_with_options(&hole[..], opts);
        match dec.decode() {
            Ok(px) => println!("hole: zune04 unexpected-ok len={} fnv={:016x}", px.len(), fnv1a(&px)),
            Err(e) => println!("hole: zune04 err = {e}"),
        }
        // 同流在 image（zune 0.5.x 后端）下正常解码 → 佐证破洞在 0.4.21 解码线
        match image::load_from_memory_with_format(&hole[..], ImageFormat::Jpeg) {
            Ok(dimg) => {
                let px = dimg.to_rgb8().into_raw();
                println!("hole: image ok {}x{} ifnv={:016x}", dimg.width(), dimg.height(), fnv1a(&px));
            }
            Err(e) => println!("hole: image err = {e}"),
        }
    }
}
