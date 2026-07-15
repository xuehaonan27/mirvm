#!/usr/bin/env mirvm
---
[dependencies]
# 核心编码器零依赖；image/svg/pic renderer 是可选 feature，本 driver 只用
# 内建 string/unicode renderer 与 to_colors 矩阵——default-features=false 砍掉
# image crate 依赖，API 面不变。
qrcode = { version = "0.14", default-features = false }
# rqrr 的 img feature 拉 image crate（不需要）；PreparedImage::prepare_from_greyscale
# 直接吃闭包灰度图。default-features=false 后只剩 g2p + lru 两个纯 Rust 依赖。
rqrr = { version = "0.9", default-features = false }
---
// qrcode 0.14 生成 + rqrr 0.9 检测解码的 QR 闭环差分。
// 覆盖：
//  ① 固定文本 × (版本,纠错级) 四档（v1L/v2M/v4Q/v5H，v5H 含 CJK UTF-8）：
//     qrcode::with_version 编码 → width/dark 计数/01 谱 FNV → 自渲染灰度图 →
//     rqrr prepare_from_greyscale → detect_grids → decode → 与原文逐字节相等
//     + MetaData(version/ecc/mask)。
//  ② QrCode::new 自动版本 + optimize.rs 模式分段（Numeric/Alphanumeric/Byte/
//     Kanji/混合五种文本）+ with_error_correction_level。
//  ③ renderer 三面：&str ██ 块渲染（含 quiet zone）、char 渲染（关 quiet zone，
//     21×21 矩阵原文打出）、unicode::Dense1x2、min_dimensions 放大。
//  ④ rqrr 图像路径：中心 5×5 模块翻转仍解码（Reed-Solomon 纠错深路径）、
//     反色图/空白图 detect 0 grid、双码同框 detect 2 grid 解码排序输出、
//     decode_to(writer) 字节级比对。
//  ⑤ 错误路径：DataTooLong、InvalidVersion（Normal 0 / Normal 41 / Micro1+H /
//     Micro4+H）；Micro QR 正常档（micro1L/micro2M/micro3M）编码。
//
// 已知坑（上游限制，语义绕行）：
//  - rqrr 0.9 检测要求模块 ≥2px：scale=1（模块=1px）会在 rqrr 内部
//    identify/grid.rs `assertion failed: scan >= 1` panic——全程 scale=2 渲染。
//  - qrcode 的 Version::Micro(0) 在上游 cast.rs 直接 panic（非 QrError），
//    不作为错误路径用例。
//  - rqrr MetaData.ecc_level 是 QR 格式信息原始 2bit（M=0/L=1/H=2/Q=3），
//    与 qrcode EcLevel 编号不同——只按原值打印，不做映射。
// 确定性：纯计算无时间/线程/哈希序；输出为计数/布尔/定值字符串/FNV hex。
use qrcode::render::unicode;
use qrcode::{Color, EcLevel, QrCode, QrResult, Version};
use rqrr::PreparedImage;

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 矩阵 01 谱（行主序，暗=1）+ 暗模块计数。
fn spectrum(code: &QrCode) -> (usize, String) {
    let colors = code.to_colors();
    let mut dark = 0usize;
    let mut s = String::with_capacity(colors.len());
    for c in &colors {
        if *c == Color::Dark {
            dark += 1;
            s.push('1');
        } else {
            s.push('0');
        }
    }
    (dark, s)
}

/// 自渲染灰度位图：暗模块=0 / 亮=255，每模块 scale×scale 像素，四周 qz 模块宽
/// quiet zone。返回 (像素缓冲, 边长)。
fn render_gray(code: &QrCode, scale: usize, qz: usize) -> (Vec<u8>, usize) {
    let n = code.width();
    let colors = code.to_colors();
    let w = (n + 2 * qz) * scale;
    let mut img = vec![255u8; w * w];
    for my in 0..n {
        for mx in 0..n {
            let v = colors[my * n + mx].select(0u8, 255u8);
            for dy in 0..scale {
                for dx in 0..scale {
                    img[((qz + my) * scale + dy) * w + (qz + mx) * scale + dx] = v;
                }
            }
        }
    }
    (img, w)
}

/// 单图检测 + 解码的摘要行。`expect` 为期望解出的原文（None = 只报 grid 数）。
fn probe(img: &[u8], w: usize, h: usize, expect: Option<&str>) -> String {
    let mut prep = PreparedImage::prepare_from_greyscale(w, h, |x, y| img[y * w + x]);
    let grids = prep.detect_grids();
    let mut out = format!("grids={}", grids.len());
    if let Some(g) = grids.first() {
        match g.decode() {
            Ok((meta, s)) => {
                let eq = expect.is_some_and(|t| s == t);
                out += &format!(
                    " eq={eq} rv={} recc={} rmask={}",
                    meta.version.0, meta.ecc_level, meta.mask
                );
            }
            Err(e) => out += &format!(" decode-err={e:?}"),
        }
    }
    out
}

/// QrResult 的错误文本（QrCode 不实现 Debug，不能 unwrap_err）。
fn es(r: QrResult<QrCode>) -> String {
    match r {
        Ok(_) => "unexpected-ok".to_string(),
        Err(e) => format!("{e:?}"),
    }
}

fn main() {
    // ---- ① 编码 × 闭环解码套件 ----
    let cases: [(&str, &str, Version, EcLevel); 4] = [
        ("v1L", "MIRVM-QR-01", Version::Normal(1), EcLevel::L),
        ("v2M", "mirvm qr round v2M", Version::Normal(2), EcLevel::M),
        (
            "v4Q",
            "mirvm qr roundtrip payload 0123456789",
            Version::Normal(4),
            EcLevel::Q,
        ),
        (
            "v5H",
            "mirvm 闭环 QR emoji-free 汉字 混合",
            Version::Normal(5),
            EcLevel::H,
        ),
    ];
    for (label, text, v, ec) in cases {
        let code = QrCode::with_version(text, v, ec).unwrap();
        let (dark, spec) = spectrum(&code);
        let (img, w) = render_gray(&code, 2, 4);
        println!(
            "{label} width={} dark={} spec={:016x} round {}",
            code.width(),
            dark,
            fnv1a(spec.as_bytes()),
            probe(&img, w, w, Some(text))
        );
    }

    // ---- ② 自动版本 + 模式分段 ----
    let modes: [(&str, &str); 5] = [
        ("numeric", "0123456789012345678901234567890123456789"),
        ("alnum", "HELLO WORLD $%*+-./: MIRVM 42"),
        ("byte-cjk", "差分测试：字节模式走 UTF-8 混合。"),
        ("kanji", "漢字漢字漢字漢字漢字漢字漢字漢字"),
        ("mixed", "ORDER-3421 漢字 mix 9876543210 tail"),
    ];
    for (label, text) in modes {
        let code = QrCode::new(text).unwrap();
        let (dark, spec) = spectrum(&code);
        println!(
            "mode/{label} width={} dark={} spec={:016x}",
            code.width(),
            dark,
            fnv1a(spec.as_bytes())
        );
    }
    let h = QrCode::with_error_correction_level("MIRVM-EC-OVERRIDE-42", EcLevel::H).unwrap();
    let (dark, spec) = spectrum(&h);
    println!(
        "ec-override width={} dark={} spec={:016x}",
        h.width(),
        dark,
        fnv1a(spec.as_bytes())
    );

    // ---- ③ renderer 三面（v1L 小码）----
    let r1 = QrCode::with_version("MIRVM-QR-01", Version::Normal(1), EcLevel::L).unwrap();
    let s_block = r1.render::<&str>().dark_color("██").light_color("  ").build();
    println!(
        "render/block lines={} len={} fnv={:016x}",
        s_block.lines().count(),
        s_block.len(),
        fnv1a(s_block.as_bytes())
    );
    let s_char = r1
        .render::<char>()
        .dark_color('#')
        .light_color('.')
        .quiet_zone(false)
        .build();
    println!(
        "render/char lines={} len={} fnv={:016x}",
        s_char.lines().count(),
        s_char.len(),
        fnv1a(s_char.as_bytes())
    );
    println!("{s_char}");
    println!("render/char-end");
    let s_uni = r1.render::<unicode::Dense1x2>().build();
    println!(
        "render/unicode lines={} len={} fnv={:016x}",
        s_uni.lines().count(),
        s_uni.len(),
        fnv1a(s_uni.as_bytes())
    );
    let s_min = r1.render::<char>().min_dimensions(40, 40).build();
    println!(
        "render/mindim lines={} len={} fnv={:016x}",
        s_min.lines().count(),
        s_min.len(),
        fnv1a(s_min.as_bytes())
    );

    // ---- ④ rqrr 图像路径（v5H 底图）----
    let ecc_text = "mirvm 闭环 QR emoji-free 汉字 混合";
    let ecc_code = QrCode::with_version(ecc_text, Version::Normal(5), EcLevel::H).unwrap();
    // 中心 5×5 模块翻转（H 级 30% 纠错余量内）→ 仍应解码出原文
    let (mut dmg, w) = render_gray(&ecc_code, 2, 4);
    for my in 10..15 {
        for mx in 10..15 {
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = ((4 + my) * 2 + dy) * w + (4 + mx) * 2 + dx;
                    dmg[p] = if dmg[p] == 0 { 255 } else { 0 };
                }
            }
        }
    }
    println!("damaged {}", probe(&dmg, w, w, Some(ecc_text)));
    // 反色图：finder 图形反转 → 检不出 grid
    let (img, w) = render_gray(&ecc_code, 2, 4);
    let inv: Vec<u8> = img.iter().map(|&b| 255 - b).collect();
    println!("inverted {}", probe(&inv, w, w, None));
    // 空白图
    let blank = vec![255u8; 64 * 64];
    println!("blank {}", probe(&blank, 64, 64, None));
    // 双码同框：v1M + v2M 水平拼接，检测 2 个 grid，解码结果排序后输出
    let g1 = QrCode::with_version("FIRST-GRID", Version::Normal(1), EcLevel::M).unwrap();
    let g2 = QrCode::with_version("SECOND GRID 22", Version::Normal(2), EcLevel::M).unwrap();
    let (i1, w1) = render_gray(&g1, 2, 4);
    let (i2, w2) = render_gray(&g2, 2, 4);
    let gap = 16;
    let (tw, th) = (w1 + gap + w2, w1.max(w2));
    let mut two = vec![255u8; tw * th];
    for y in 0..w1 {
        for x in 0..w1 {
            two[y * tw + x] = i1[y * w1 + x];
        }
    }
    for y in 0..w2 {
        for x in 0..w2 {
            two[y * tw + w1 + gap + x] = i2[y * w2 + x];
        }
    }
    let mut prep = PreparedImage::prepare_from_greyscale(tw, th, |x, y| two[y * tw + x]);
    let grids = prep.detect_grids();
    let mut decoded: Vec<String> = grids
        .iter()
        .map(|g| {
            g.decode()
                .map(|(_, s)| s)
                .unwrap_or_else(|e| format!("ERR{e:?}"))
        })
        .collect();
    decoded.sort();
    println!("two-grid n={} decoded={decoded:?}", grids.len());
    // decode_to(writer) 字节级比对 + into_colors 消费 API
    let mut prep = PreparedImage::prepare_from_greyscale(w, w, |x, y| img[y * w + x]);
    let grids = prep.detect_grids();
    let mut out = Vec::new();
    let meta = grids[0].decode_to(&mut out).unwrap();
    let colors = ecc_code.into_colors();
    println!(
        "decode_to bytes={} eq={} rv={} colors={}",
        out.len(),
        out == ecc_text.as_bytes(),
        meta.version.0,
        colors.len()
    );

    // ---- ⑤ 错误路径 + Micro QR ----
    println!(
        "err/toolong {}",
        es(QrCode::with_version(
            "this payload is way too long for version one",
            Version::Normal(1),
            EcLevel::L
        ))
    );
    println!("err/v0 {}", es(QrCode::with_version("x", Version::Normal(0), EcLevel::L)));
    println!("err/v41 {}", es(QrCode::with_version("x", Version::Normal(41), EcLevel::M)));
    println!(
        "err/micro1H {}",
        es(QrCode::with_version("1", Version::Micro(1), EcLevel::H))
    );
    println!(
        "err/micro4H {}",
        es(QrCode::with_version("12345", Version::Micro(4), EcLevel::H))
    );
    for (mv, ec) in [(1, EcLevel::L), (2, EcLevel::M), (3, EcLevel::M)] {
        let c = QrCode::with_version("12345", Version::Micro(mv), ec).unwrap();
        let (dark, spec) = spectrum(&c);
        println!(
            "micro{mv} width={} dark={} spec={:016x}",
            c.width(),
            dark,
            fnv1a(spec.as_bytes())
        );
    }
}
