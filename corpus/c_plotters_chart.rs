#!/usr/bin/env mirvm
---
[dependencies]
# plotters 0.3.7 精简 feature 集（浮点重图表渲染差分）。裁剪理由：
# 1) 默认 ttf = font-kit + ttf-parser + lazy_static + pathfinder_geometry；
#    font-kit 在 Linux 走系统 fontconfig/字体扫描（环境重依赖，违单文件纪律）。
#    关 ttf 后 plotters 用内置 naive 等宽字量度（src/style/font/naive.rs：
#    estimate_layout 为 size/1.24/1.24 等纯 f64 算术），SVG 侧 <text> 坐标
#    两侧同码、位型确定，可差分。
# 2) 默认 bitmap_encoder/bitmap_gif 拖 image/gif/jpeg 大依赖树；纯内存 RGB
#    buffer 差分只需 bitmap_backend（零额外依赖）。
# 最终 dep tree 仅 5 件：plotters + plotters-{backend,svg,bitmap} + num-traits。
plotters = { version = "=0.3.7", default-features = false, features = [
    "svg_backend",
    "bitmap_backend",
    "line_series",
    "point_series",
    "area_series",
    "histogram",
] }
---
// plotters 0.3.7 图表渲染差分：SVG backend（文本即输出，自打印便对拍）+
// BitMap backend（320x240 RGB 内存帧缓冲，像素 FNV）。固定数据序列，全程
// 重浮点：坐标映射 f64→i32、key_points 的 powf/log10/floor 循环、log_scale
// 轴的 ln/exp、area/histogram 几何。
//
// 覆盖：
// ① SVG 折线×2 + 散点（Circle filled / Cross）+ 网格 + caption + legend +
//    轴 desc/labels（naive 字量度路径）+ backend_coord 锚点；
// ② SVG 柱状：分段整数轴 (into_segmented) × 三样式 fill + style_func（按
//    SegmentValue::Exact/CenterOf 分色描边）+ baseline + margin 双档；
// ③ SVG 双区 split：左 log_scale y 轴折线+TriangleMarker（ln/exp 路径），
//    右 AreaSeries mix(0.35) 半透明填充 + border_style；
// ④ 边界：空 LineSeries/PointSeries（f64 轴）、空 Histogram（i32 离散轴；
//    float range 无 DiscreteRanged 实现）、零宽 range（1.0..1.0，map 走
//    corner-case 早退）、反转 range（4..0）；
// ⑤ BitMap 320x240：网格线 + 逐柱 Histogram + AreaSeries + LineSeries +
//    Circle 点的复合场景，像素 FNV/非白像素数/抽样像素 hex；
// ⑥ 错误路径：with_buffer_and_format 小缓冲 Err；bitmap 后端画文字。
//
// 已知绕行记录（语义覆盖不变，绕的是 plotters 自身的非确定源）：
// A) Histogram::data 内部以 std HashMap<usize, A> 聚合、into_iter() 桶序出
//    结果——多桶时绘制顺序依赖 RandomState（进程间随机；12 键小例实测三次
//    三序）。绕行 = 多柱图按柱逐次 draw_series（每个 Histogram 数据塌缩为单
//    子键，迭代序平凡确定），聚合语义由同一子键喂多项 [(x,0),(x,v),(x,1)]
//    覆盖；SVG/像素输出两侧逐字节一致。
// B) 无 ttf 时 BitMap 后端的 draw_text 落 FontData::draw 默认实现——无条件
//    panic("The font implementation is unable to draw text")。故 bitmap 图
//    不设 label area（label area 为 None 时 draw_axis_and_labels 直接早退，
//    零 draw_text）、不画 caption/legend/desc；末尾以静默 hook + catch_unwind
//    显式断言该 panic 两侧一致发生（成功路径 stderr 仍为空）。
//
// 确定性：数据为整数×2^-k（二进制精确，任何正确 FP 实现两侧位型一致）/
// 定种 xorshift64*；无时间/地址/HashMap 序；二进制输出打印 长度+FNV-1a；
// 浮点一律 to_bits() 锁位。
use plotters::backend::RGBPixel;
use plotters::prelude::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 定种 xorshift64*（native/mirvm 同序列）。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

/// 折线数据：n 点，x = i/4（2^-2 精确），y = 整数×2^-3。
fn line_data(seed: u64, n: u64, center: i64) -> Vec<(f64, f64)> {
    let mut r = Rng(seed);
    (0..n)
        .map(|i| {
            let x = i as f64 / 4.0;
            let y = (center + (r.next() % 33) as i64 - 16) as f64 / 8.0;
            (x, y)
        })
        .collect()
}

/// 散点数据：x ∈ [0,6] 步进 1/4，y ∈ [-2.5, 3.5] 步进 1/8。
fn scatter_data(seed: u64, n: u64) -> Vec<(f64, f64)> {
    let mut r = Rng(seed);
    (0..n)
        .map(|_| {
            let x = (r.next() % 25) as f64 / 4.0;
            let y = ((r.next() % 49) as i64 - 20) as f64 / 8.0;
            (x, y)
        })
        .collect()
}

/// 柱数据：i ∈ [0,n)，x = 2i，value ∈ [2,36)（i32 离散轴用）。
fn bars_data(seed: u64, n: i32) -> Vec<(i32, i32)> {
    let mut r = Rng(seed);
    (0..n).map(|i| (i * 2, 2 + (r.next() % 34) as i32)).collect()
}

fn print_svg(tag: &str, svg: &str) {
    println!("{tag} len={} fnv={:016x}", svg.len(), fnv1a(svg.as_bytes()));
    print!("--{tag}--\n{svg}");
    if !svg.ends_with('\n') {
        println!();
    }
    println!("--/{tag}--");
}

/// 描边样式速构（ShapeStyle 非 Color，不能走 Into<ShapeStyle> 引用路径）。
fn sw(color: RGBAColor, width: u32) -> ShapeStyle {
    ShapeStyle {
        color,
        filled: false,
        stroke_width: width,
    }
}

fn main() {
    let la = line_data(0x9E37_79B9_7F4A_7C15, 24, 12);
    let lb = line_data(0xA3C5_9C3D_5F0A_1122, 24, -2);
    let scat = scatter_data(0xDEAD_BEEF_CAFE_F00D, 18);
    let bars = bars_data(0x0BAD_F00D_ABBA_2024, 10);
    for (tag, d) in [("la", &la), ("lb", &lb), ("scat", &scat)] {
        let flat: String = d
            .iter()
            .map(|p| format!("({},{});", p.0.to_bits(), p.1.to_bits()))
            .collect();
        println!("{tag} n={} bits-fnv={:016x}", d.len(), fnv1a(flat.as_bytes()));
    }
    println!(
        "bars n={} sum={}",
        bars.len(),
        bars.iter().map(|b| b.1).sum::<i32>()
    );

    // ① SVG 折线+散点+网格+caption+legend（全要素）
    let mut svg1 = String::new();
    {
        let root = SVGBackend::with_string(&mut svg1, (480, 300)).into_drawing_area();
        root.fill(&WHITE).unwrap();
        let mut chart = ChartBuilder::on(&root)
            .margin(6)
            .caption("line+scatter", ("sans-serif", 20))
            .x_label_area_size(26)
            .y_label_area_size(36)
            .build_cartesian_2d(0.0f64..6.0f64, -2.5f64..3.5f64)
            .unwrap();
        chart
            .configure_mesh()
            .x_desc("xs")
            .y_desc("val")
            .x_labels(13)
            .y_labels(7)
            .axis_desc_style(("sans-serif", 13).into_font().into_text_style(&root))
            .draw()
            .unwrap();
        let sa = sw(RED.mix(0.9), 2);
        chart
            .draw_series(LineSeries::new(la.iter().copied(), sa))
            .unwrap()
            .label("serA")
            .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 14, y)], sa));
        let sb = sw(BLUE.mix(0.6), 1);
        chart
            .draw_series(LineSeries::new(lb.iter().copied(), sb))
            .unwrap()
            .label("serB")
            .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 14, y)], sb));
        chart
            .draw_series(PointSeries::of_element(
                scat.iter().copied(),
                3,
                &GREEN,
                &|c, s, st| Circle::new(c, s, st.filled()),
            ))
            .unwrap();
        chart
            .draw_series(PointSeries::of_element(
                scat.iter().step_by(3).copied(),
                5,
                &MAGENTA,
                &|c, s, st| Cross::new(c, s, st),
            ))
            .unwrap();
        chart
            .configure_series_labels()
            .position(SeriesLabelPosition::UpperRight)
            .background_style(WHITE.mix(0.85))
            .border_style(&BLACK)
            .draw()
            .unwrap();
        for (i, p) in [(0.25f64, -1.0f64), (3.0, 0.0), (5.75, 2.5)]
            .iter()
            .enumerate()
        {
            let (px, py) = chart.backend_coord(p);
            println!("c1 bc{i}=({px},{py})");
        }
        root.present().unwrap();
    }
    print_svg("svg1", &svg1);

    // ② SVG 柱状（分段整数 x 轴，三档样式 + style_func/baseline/margin）
    let mut svg2 = String::new();
    {
        let root = SVGBackend::with_string(&mut svg2, (420, 260)).into_drawing_area();
        root.fill(&WHITE).unwrap();
        let mut chart = ChartBuilder::on(&root)
            .margin(6)
            .caption("bars", ("sans-serif", 18))
            .x_label_area_size(24)
            .y_label_area_size(32)
            .build_cartesian_2d((-1..20).into_segmented(), 0..40)
            .unwrap();
        chart
            .configure_mesh()
            .x_labels(21)
            .y_labels(9)
            .y_desc("cnt")
            .draw()
            .unwrap();
        // 逐柱 draw_series：单 Histogram 塌缩单子键（HashMap 序绕行，见头注 A）
        for (i, &(x, v)) in bars.iter().enumerate() {
            let st = match i % 3 {
                0 => ShapeStyle {
                    color: GREEN.mix(0.75),
                    filled: true,
                    stroke_width: 1,
                },
                1 => ShapeStyle {
                    color: CYAN.mix(0.65),
                    filled: true,
                    stroke_width: 1,
                },
                _ => ShapeStyle {
                    color: MAGENTA.mix(0.55),
                    filled: true,
                    stroke_width: 1,
                },
            };
            chart
                .draw_series(
                    Histogram::vertical(&chart)
                        .style(st)
                        .margin(3)
                        .data([(x, 0), (x, v), (x, 1)]),
                )
                .unwrap();
        }
        chart
            .draw_series(
                Histogram::vertical(&chart)
                    .style_func(|v, _| match v {
                        SegmentValue::Exact(_) => sw(BLACK.to_rgba(), 2),
                        _ => sw(RED.to_rgba(), 2),
                    })
                    .baseline(5)
                    .margin(6)
                    .data([(1, 30i32), (1, 4), (1, 2)]),
            )
            .unwrap();
        let (bx, by) = chart.backend_coord(&(SegmentValue::Exact(6), 20));
        let (cx, cy) = chart.backend_coord(&(SegmentValue::CenterOf(6), 20));
        println!("c2 seg-exact bc=({bx},{by}) seg-center bc=({cx},{cy})");
        root.present().unwrap();
    }
    print_svg("svg2", &svg2);

    // ③ SVG 双区：log_scale y 轴折线 + AreaSeries 半透明
    let mut svg3 = String::new();
    {
        let root = SVGBackend::with_string(&mut svg3, (520, 220)).into_drawing_area();
        root.fill(&WHITE).unwrap();
        let subs = root.split_evenly((1, 2));
        // y = 1.25^i + frac（ln/exp 与浮点累加压力；值 > log 轴 zero_point 0.8）
        let log_pts: Vec<(f32, f64)> = {
            let mut r = Rng(0x0123_4567_89AB_CDEF);
            (0..9)
                .map(|i| {
                    let x = i as f32 * 1.25;
                    let y = 1.25f64.powi(i as i32) + (r.next() % 4) as f64 / 4.0;
                    (x, y)
                })
                .collect()
        };
        let mut chart = ChartBuilder::on(&subs[0])
            .margin(4)
            .x_label_area_size(20)
            .y_label_area_size(38)
            .build_cartesian_2d(0.0f32..10.0f32, (0.8f64..500.0f64).log_scale())
            .unwrap();
        chart
            .configure_mesh()
            .x_labels(6)
            .y_labels(6)
            .draw()
            .unwrap();
        chart
            .draw_series(LineSeries::new(log_pts.iter().copied(), sw(RED.to_rgba(), 2)))
            .unwrap();
        chart
            .draw_series(PointSeries::of_element(
                log_pts.iter().step_by(2).copied(),
                4,
                &BLUE,
                &|c, s, st| TriangleMarker::new(c, s, st.filled()),
            ))
            .unwrap();
        let (lx, ly) = chart.backend_coord(&(5.0f32, 10.0f64));
        let (lx2, ly2) = chart.backend_coord(&(2.5f32, 100.0f64));
        println!("c3-log bc1=({lx},{ly}) bc2=({lx2},{ly2})");

        let area_data: Vec<(f64, f64)> = (0..17u64)
            .map(|i| {
                let x = i as f64 / 2.0;
                let y = 2.0 + (((i * 7) % 9) as i64 - 4) as f64 / 4.0;
                (x, y)
            })
            .collect();
        let mut chart = ChartBuilder::on(&subs[1])
            .margin(4)
            .x_label_area_size(20)
            .y_label_area_size(28)
            .build_cartesian_2d(0.0f64..8.0f64, 0.0f64..4.0f64)
            .unwrap();
        chart
            .configure_mesh()
            .x_labels(9)
            .y_labels(5)
            .draw()
            .unwrap();
        chart
            .draw_series(
                AreaSeries::new(area_data.iter().copied(), 0.0, BLUE.mix(0.35))
                    .border_style(sw(BLUE.to_rgba(), 2)),
            )
            .unwrap();
        let (ax, ay) = chart.backend_coord(&(4.0f64, 3.5f64));
        println!("c3-area bc=({ax},{ay})");
        root.present().unwrap();
    }
    print_svg("svg3", &svg3);

    // ④ 边界：空 series / 零宽 range / 反转 range
    let mut svge = String::new();
    {
        // 每张子图独立成段：ChartContext/DrawingArea 均持 &mut svge 且实现 Drop，
        // 需显式 drop 释放借用再开下一张。
        let root = SVGBackend::with_string(&mut svge, (300, 200)).into_drawing_area();
        root.fill(&WHITE).unwrap();
        let mut chart = ChartBuilder::on(&root)
            .margin(4)
            .x_label_area_size(20)
            .y_label_area_size(28)
            .build_cartesian_2d(0.0f64..4.0f64, -1.0f64..1.0f64)
            .unwrap();
        chart.configure_mesh().draw().unwrap();
        let empty_line: Vec<(f64, f64)> = Vec::new();
        let ok1 = chart
            .draw_series(LineSeries::new(empty_line, &RED))
            .map(|_| ())
            .is_ok();
        let ok3 = chart
            .draw_series(PointSeries::of_element(
                Vec::<(f64, f64)>::new(),
                2,
                &GREEN,
                &|c, s, st| Circle::new(c, s, st),
            ))
            .map(|_| ())
            .is_ok();
        println!("edge empty-line={ok1} empty-pts={ok3}");
        root.present().unwrap();
        drop(chart);
        drop(root);

        // 空 Histogram 需离散 x 轴（float range 无 DiscreteRanged 实现）
        let root_h = SVGBackend::with_string(&mut svge, (200, 160)).into_drawing_area();
        root_h.fill(&WHITE).unwrap();
        let mut ch = ChartBuilder::on(&root_h)
            .margin(4)
            .build_cartesian_2d(0..5i32, 0..10i32)
            .unwrap();
        ch.configure_mesh().draw().unwrap();
        let ok2 = ch
            .draw_series(
                Histogram::vertical(&ch)
                    .style(&CYAN)
                    .data(Vec::<(i32, i32)>::new()),
            )
            .map(|_| ())
            .is_ok();
        println!("edge empty-hist={ok2}");
        root_h.present().unwrap();
        drop(ch);
        drop(root_h);

        // 零宽 range：Ranged::map 走 `self.1 == self.0` corner-case 早退（中点）
        let root2 = SVGBackend::with_string(&mut svge, (200, 160)).into_drawing_area();
        root2.fill(&WHITE).unwrap();
        let mut cz = ChartBuilder::on(&root2)
            .margin(4)
            .build_cartesian_2d(1.0f64..1.0f64, 1.0f64..1.0f64)
            .unwrap();
        cz.configure_mesh().draw().unwrap();
        let (zx, zy) = cz.backend_coord(&(1.0f64, 1.0f64));
        let oz = cz
            .draw_series(PointSeries::of_element(
                [(1.0f64, 1.0f64)],
                3,
                &RED,
                &|c, s, st| Circle::new(c, s, st.filled()),
            ))
            .map(|_| ())
            .is_ok();
        println!("zero-range map=({zx},{zy}) draw={oz}");
        root2.present().unwrap();
        drop(cz);
        drop(root2);

        // 反转 range：双轴均递减（map 走 actual_length < 0 的 ceil 分支）
        let root3 = SVGBackend::with_string(&mut svge, (200, 160)).into_drawing_area();
        root3.fill(&WHITE).unwrap();
        let mut cr = ChartBuilder::on(&root3)
            .margin(4)
            .build_cartesian_2d(4.0f64..0.0f64, 1.0f64..0.0f64)
            .unwrap();
        cr.configure_mesh().draw().unwrap();
        let (rx, ry) = cr.backend_coord(&(3.0f64, 0.25f64));
        let or = cr
            .draw_series(LineSeries::new(
                [(0.5f64, 0.0f64), (1.5, 0.75), (3.5, 0.25)],
                &BLUE,
            ))
            .map(|_| ())
            .is_ok();
        println!("rev-range map=({rx},{ry}) draw={or}");
        root3.present().unwrap();
        drop(cr);
        drop(root3);
    }
    print_svg("svge", &svge);

    // ⑤ BitMap 320x240 RGB 帧缓冲（全程零文字：label area 未设 → None → 早退）
    {
        let (w, h) = (320u32, 240u32);
        let mut buf = vec![0u8; (w * h * 3) as usize];
        {
            let root = BitMapBackend::with_buffer(&mut buf, (w, h)).into_drawing_area();
            root.fill(&WHITE).unwrap();
            let mut chart = ChartBuilder::on(&root)
                .margin(5)
                .build_cartesian_2d(-1..20i32, 0..40i32)
                .unwrap();
            chart
                .configure_mesh()
                .x_labels(8)
                .y_labels(9)
                .disable_x_mesh()
                .bold_line_style(WHITE.mix(0.5))
                .draw()
                .unwrap();
            for (i, &(x, v)) in bars.iter().enumerate() {
                let st = if i % 2 == 0 {
                    ShapeStyle {
                        color: BLUE.mix(0.6),
                        filled: true,
                        stroke_width: 1,
                    }
                } else {
                    ShapeStyle {
                        color: GREEN.mix(0.7),
                        filled: true,
                        stroke_width: 1,
                    }
                };
                chart
                    .draw_series(
                        Histogram::vertical(&chart)
                            .style(st)
                            .margin(4)
                            .data([(x, 0), (x, v)]),
                    )
                    .unwrap();
            }
            let li: Vec<(i32, i32)> = line_data(0x77AA_F00D_1357_9BDF, 12, 20)
                .into_iter()
                .map(|(x, y)| ((x * 2.0) as i32, (y * 8.0) as i32))
                .collect();
            chart
                .draw_series(
                    AreaSeries::new(li.iter().copied(), 0, RED.mix(0.25))
                        .border_style(sw(RED.to_rgba(), 1)),
                )
                .unwrap();
            chart
                .draw_series(LineSeries::new(li.iter().copied(), sw(BLACK.to_rgba(), 2)))
                .unwrap();
            chart
                .draw_series(PointSeries::of_element(
                    li.iter().step_by(2).copied(),
                    3,
                    &MAGENTA,
                    &|c, s, st| Circle::new(c, s, st.filled()),
                ))
                .unwrap();
            root.present().unwrap();
        }
        println!("bmp {}x{} fnv={:016x}", w, h, fnv1a(&buf));
        let nonwhite = buf.chunks(3).filter(|p| *p != [255, 255, 255]).count();
        println!("bmp nonwhite={nonwhite} total={}", buf.len() / 3);
        for (x, y) in [(0u32, 0u32), (40, 30), (80, 120), (160, 200), (300, 40), (319, 239)] {
            let o = ((y * w + x) * 3) as usize;
            println!(
                "bmp px({x},{y})={:02x}{:02x}{:02x}",
                buf[o],
                buf[o + 1],
                buf[o + 2]
            );
        }
    }

    // ⑥ 错误路径：小缓冲 Err（InvalidBuffer）；bitmap 画文字必 panic（见头注 B）
    let mut small = vec![0u8; 64];
    match BitMapBackend::<RGBPixel>::with_buffer_and_format(&mut small, (320, 240)) {
        Ok(_) => println!("small-buf unexpected ok"),
        Err(e) => println!("small-buf err: {e}"),
    }
    std::panic::set_hook(Box::new(|_| {}));
    let mut buf2 = vec![0u8; 64 * 64 * 3];
    let r = catch_unwind(AssertUnwindSafe(|| {
        let root = BitMapBackend::with_buffer(&mut buf2, (64, 64)).into_drawing_area();
        let style: TextStyle = FontDesc::new(FontFamily::SansSerif, 12.0, FontStyle::Normal).into();
        root.draw_text("t", &style, (2, 2)).unwrap();
    }));
    match r {
        Ok(()) => println!("bitmap-text no-panic"),
        Err(e) => {
            let msg = e
                .downcast_ref::<&'static str>()
                .map(|s| (*s).to_owned())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "?".to_string());
            println!("bitmap-text panic=true msg={msg}");
        }
    }
    println!("done");
}
