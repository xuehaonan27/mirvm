#!/usr/bin/env mirvm
---
[dependencies]
# plotters 0.3.7 trimmed feature set (heavy-float chart rendering differential). Why:
# 1) the default ttf = font-kit + ttf-parser + lazy_static + pathfinder_geometry;
#    font-kit uses the system fontconfig/font scan on Linux (heavy env dependency,
#    violating single-file discipline). With ttf off plotters uses its built-in naive
#    monospace metric (src/style/font/naive.rs: estimate_layout is pure f64 like
#    size/1.24/1.24), so SVG <text> coords match and bit patterns are deterministic.
# 2) the default bitmap_encoder/bitmap_gif drag in a large image/gif/jpeg tree; an
#    in-memory RGB buffer differential needs only bitmap_backend (zero extra deps).
# Final dep tree is just 5 crates: plotters + plotters-{backend,svg,bitmap} + num-traits.
plotters = { version = "=0.3.7", default-features = false, features = [
    "svg_backend",
    "bitmap_backend",
    "line_series",
    "point_series",
    "area_series",
    "histogram",
] }
---
// plotters 0.3.7 chart rendering differential: SVG backend (text is the output, so
// it prints itself for comparison) + BitMap backend (320x240 RGB in-memory frame
// buffer, pixel FNV). Fixed data series, heavy float throughout: coordinate mapping
// f64->i32, key_points powf/log10/floor loop, log_scale axis ln/exp, area/histogram.
//
// Coverage:
// ① SVG polyline x2 + scatter (Circle filled / Cross) + mesh + caption + legend +
//    axis desc/labels (naive font metric path) + backend_coord anchors;
// ② SVG bars: segmented integer axis (into_segmented) x three fill styles + style_func
//    (per SegmentValue::Exact/CenterOf colored stroke) + baseline + two margin tiers;
// ③ SVG two-area split: left log_scale y-axis polyline + TriangleMarker (ln/exp path),
//    right AreaSeries mix(0.35) translucent fill + border_style;
// ④ boundaries: empty LineSeries/PointSeries (f64 axis), empty Histogram (i32 discrete
//    axis; float range has no DiscreteRanged impl), zero-width range (1.0..1.0, map
//    takes the corner-case early return), reversed range (4..0);
// ⑤ BitMap 320x240: mesh lines + per-bar Histogram + AreaSeries + LineSeries +
//    Circle points composite scene, pixel FNV/non-white pixel count/sampled pixel hex;
// ⑥ error paths: with_buffer_and_format small buffer Err; bitmap backend draws text.
//
// Known workarounds (coverage unchanged; each avoids a plotters-internal nondeterminism source):
// A) Histogram::data aggregates in a std HashMap<usize, A>, so into_iter() bucket
//    order (hence multi-bucket draw order) depends on RandomState; workaround = draw
//    multi-bar charts bar by bar via draw_series (one subkey per Histogram,
//    trivially deterministic iteration order), feeding one subkey several items to
//    cover aggregation. SVG/pixel output is byte-identical on both sides.
// B) Without ttf, BitMap draw_text falls back to FontData::draw's default impl,
//    which panics unconditionally ("The font implementation is unable to draw
//    text"). So bitmap charts set no label area (None -> draw_axis_and_labels
//    early-returns, zero draw_text) and draw no caption/legend/desc; a silent hook +
//    catch_unwind at the end asserts that panic on both sides (stderr empty on success).
//
// Determinism: data is integer x 2^-k (exact in binary, so any correct FP
// implementation gives identical bit patterns on both sides) / seeded xorshift64*;
// no time/address/HashMap order; binary output is length + FNV-1a; floats always to_bits().
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

/// Seeded xorshift64* (same sequence on native/mirvm).
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

/// Polyline data: n points, x = i/4 (exact, 2^-2), y = integer x 2^-3.
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

/// Scatter data: x in [0,6] step 1/4, y in [-2.5, 3.5] step 1/8.
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

/// Bar data: i in [0,n), x = 2i, value in [2,36) (for the i32 discrete axis).
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

/// Quick stroke-style constructor (ShapeStyle is not Color, so the Into<ShapeStyle> path is unavailable).
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

    // ① SVG polyline+scatter+mesh+caption+legend (all elements)
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

    // ② SVG bars (segmented integer x axis, three styles + style_func/baseline/margin)
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
        // Per-bar draw_series: one Histogram collapses to a single subkey (HashMap-order workaround, note A)
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

    // ③ SVG two areas: log_scale y-axis polyline + translucent AreaSeries
    let mut svg3 = String::new();
    {
        let root = SVGBackend::with_string(&mut svg3, (520, 220)).into_drawing_area();
        root.fill(&WHITE).unwrap();
        let subs = root.split_evenly((1, 2));
        // y = 1.25^i + frac (ln/exp and float-accumulation pressure; values > log axis zero_point 0.8)
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

    // ④ boundaries: empty series / zero-width range / reversed range
    let mut svge = String::new();
    {
        // Each subplot is its own block: ChartContext/DrawingArea both hold &mut svge and
        // implement Drop, so the borrow must be released with an explicit drop before the next.
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

        // Empty Histogram needs a discrete x axis (float range has no DiscreteRanged impl)
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

        // Zero-width range: Ranged::map takes the `self.1 == self.0` corner-case early return (midpoint)
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

        // Reversed range: both axes decrease (map takes the actual_length < 0 ceil branch)
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

    // ⑤ BitMap 320x240 RGB frame buffer (zero text throughout: label area unset -> None -> early return)
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

    // ⑥ error paths: small buffer Err (InvalidBuffer); bitmap draw_text must panic (see header note B)
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
