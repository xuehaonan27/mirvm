#!/usr/bin/env mirvm
---
[dependencies]
lyon = "1"
---
// lyon 1.0（lyon_tessellation 1.0.20 / lyon_path 1.0.19 / lyon_algorithms 1.0.19）
// 2D 镶嵌浮点重差分。
// 固定路径集：complex（三次+二次贝塞尔+SVG 端点弧 arc_to+自交 S 曲线，svg_builder）、
// selfint（领结四边形 + 大环三次曲线闭合自交）、holed（外环 + 反向内环成孔 +
// 第三环叠压，区分 EvenOdd/NonZero）、open（开放折线+二次贝塞尔，stroke 主体）、
// sharp（≈3.8° 浅转角，miter limit 回退矩阵）。
// Fill：EvenOdd/NonZero × complex/selfint/holed；tolerance 0.1/0.01；Vertical/
// Horizontal 扫描向；handle_intersections=false 作用于自交路径；基本形状 fast
// path tessellate_rectangle/circle/ellipse。
// Stroke：width 0.5/2/8；join Miter/MiterClip/Round/Bevel + miter_limit 1.0；
// cap Butt/Square/Round + 首尾异 cap；dash 两条（lyon 1.x StrokeOptions 已无内建
// dash，用 walk_along_path+RepeatedPattern 手切虚线子路径再 stroke）；
// variable_line_width 自定义属性路径；complex 闭环 Round join 大输出 fnv 锚。
// 错误/退化：tolerance NaN 与 0.0（ToleranceIsNaN）、原始事件流带 NaN 坐标
// （PositionIsNaN）、空路径、孤立点（fill 空 / stroke 三 cap：Butt 空、
// Square 成方、Round 成圆）、零长线段、全重合点。
// 输出：每次镶嵌 nv/ni/输出序 FNV-1a（顶点全字段 bits + 索引值）/三角形总面积
// to_bits；nv≤24 按输出序逐顶点打印坐标 bits，否则打 v0..v2 与末顶点。
// 确定性：全固定常量；f32 一律 to_bits；无 HashMap/时间/线程/地址。
use lyon::algorithms::walk::{walk_along_path, RepeatedPattern, WalkerEvent};
use lyon::math::{point, vector, Angle, Box2D, Point};
use lyon::path::builder::SvgPathBuilder;
use lyon::path::{ArcFlags, Path, PathEvent, Winding};
use lyon::tessellation::geometry_builder::{BuffersBuilder, VertexBuffers};
use lyon::tessellation::{
    FillGeometryBuilder, FillOptions, FillRule, FillTessellator, FillVertex,
    FillVertexConstructor, LineCap, LineJoin, Orientation, StrokeOptions, StrokeTessellator,
    StrokeVertex, StrokeVertexConstructor, TessellationResult,
};

fn mix32(h: &mut u64, v: u32) {
    for b in v.to_le_bytes() {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn mixp(h: &mut u64, p: Point) {
    mix32(h, p.x.to_bits());
    mix32(h, p.y.to_bits());
}

/// 输出顶点抽象：把全字段混进 FNV-1a，并提供坐标供逐顶点 bits 打印。
trait Out {
    fn mix(&self, h: &mut u64);
    fn xy(&self) -> (f32, f32);
}

/// fill 顶点：位置 + as_endpoint_id（自交点/曲线内部为 u32::MAX）。
#[derive(Copy, Clone)]
struct FVert {
    x: f32,
    y: f32,
    ep: u32,
}

impl Out for FVert {
    fn mix(&self, h: &mut u64) {
        mix32(h, self.x.to_bits());
        mix32(h, self.y.to_bits());
        mix32(h, self.ep);
    }
    fn xy(&self) -> (f32, f32) {
        (self.x, self.y)
    }
}

/// stroke 顶点：位置 + 法线 + advancement + 实际线宽 + side。
#[derive(Copy, Clone)]
struct SVert {
    x: f32,
    y: f32,
    nx: f32,
    ny: f32,
    adv: f32,
    lw: f32,
    side: u32,
}

impl Out for SVert {
    fn mix(&self, h: &mut u64) {
        mix32(h, self.x.to_bits());
        mix32(h, self.y.to_bits());
        mix32(h, self.nx.to_bits());
        mix32(h, self.ny.to_bits());
        mix32(h, self.adv.to_bits());
        mix32(h, self.lw.to_bits());
        mix32(h, self.side);
    }
    fn xy(&self) -> (f32, f32) {
        (self.x, self.y)
    }
}

struct FCtor;

impl FillVertexConstructor<FVert> for FCtor {
    fn new_vertex(&mut self, v: FillVertex) -> FVert {
        FVert {
            x: v.position().x,
            y: v.position().y,
            ep: v.as_endpoint_id().map(|id| id.0).unwrap_or(u32::MAX),
        }
    }
}

struct SCtor;

impl StrokeVertexConstructor<SVert> for SCtor {
    fn new_vertex(&mut self, v: StrokeVertex) -> SVert {
        SVert {
            x: v.position().x,
            y: v.position().y,
            nx: v.normal().x,
            ny: v.normal().y,
            adv: v.advancement(),
            lw: v.line_width(),
            side: v.side().to_f32().to_bits(),
        }
    }
}

/// 索引序三角形有向面积和（f32 坐标进 f64 累加，位型确定）。
fn area<V: Out>(b: &VertexBuffers<V, u32>) -> f64 {
    let mut acc = 0.0f64;
    for t in b.indices.chunks_exact(3) {
        let (ax, ay) = b.vertices[t[0] as usize].xy();
        let (bx, by) = b.vertices[t[1] as usize].xy();
        let (cx, cy) = b.vertices[t[2] as usize].xy();
        let cross = (bx - ax) * (cy - ay) - (cx - ax) * (by - ay);
        acc += (cross as f64) * 0.5;
    }
    acc
}

fn dump<V: Out>(label: &str, b: &VertexBuffers<V, u32>) {
    let mut h = 0xcbf29ce484222325u64;
    for v in &b.vertices {
        v.mix(&mut h);
    }
    for &i in &b.indices {
        mix32(&mut h, i);
    }
    println!(
        "{label} ok nv={} ni={} fnv={h:016x} area={:016x}",
        b.vertices.len(),
        b.indices.len(),
        area(b).to_bits()
    );
    let n = b.vertices.len();
    let show: Vec<usize> = if n <= 24 {
        (0..n).collect()
    } else {
        vec![0, 1, 2, n - 1]
    };
    for i in show {
        let (x, y) = b.vertices[i].xy();
        println!("{label} v{i} {:08x}:{:08x}", x.to_bits(), y.to_bits());
    }
}

fn out<V: Out>(label: &str, r: TessellationResult, b: &VertexBuffers<V, u32>) {
    match r {
        Ok(()) => dump(label, b),
        Err(e) => println!("{label} ERR {e}"),
    }
}

fn fill(t: &mut FillTessellator, label: &str, path: &Path, options: &FillOptions) {
    let mut buffers: VertexBuffers<FVert, u32> = VertexBuffers::new();
    let r = {
        let mut gb = BuffersBuilder::new(&mut buffers, FCtor);
        t.tessellate_path(path, options, &mut gb)
    };
    out(label, r, &buffers);
}

/// 原始事件流直送（绕过 Path builder 的 debug_assert NaN 检查，测试 tessellator
/// 自身的参数校验错误路径）。
fn fill_events(t: &mut FillTessellator, label: &str, events: &[PathEvent], options: &FillOptions) {
    let mut buffers: VertexBuffers<FVert, u32> = VertexBuffers::new();
    let r = {
        let mut gb = BuffersBuilder::new(&mut buffers, FCtor);
        t.tessellate(events.iter().copied(), options, &mut gb)
    };
    out(label, r, &buffers);
}

/// 基本形状 fast path（rectangle/circle/ellipse 不经 Path 数据结构）。
fn fill_shape<F>(t: &mut FillTessellator, label: &str, options: &FillOptions, f: F)
where
    F: FnOnce(&mut FillTessellator, &FillOptions, &mut dyn FillGeometryBuilder) -> TessellationResult,
{
    let mut buffers: VertexBuffers<FVert, u32> = VertexBuffers::new();
    let r = {
        let mut gb = BuffersBuilder::new(&mut buffers, FCtor);
        f(t, options, &mut gb)
    };
    out(label, r, &buffers);
}

fn stroke(t: &mut StrokeTessellator, label: &str, path: &Path, options: &StrokeOptions) {
    let mut buffers: VertexBuffers<SVert, u32> = VertexBuffers::new();
    let r = {
        let mut gb = BuffersBuilder::new(&mut buffers, SCtor);
        t.tessellate_path(path, options, &mut gb)
    };
    out(label, r, &buffers);
}

/// 输出序事件流指纹（tag + 全部坐标 bits）。
fn path_fnv(path: &Path) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for e in path.iter() {
        match e {
            PathEvent::Begin { at } => {
                mix32(&mut h, 0);
                mixp(&mut h, at);
            }
            PathEvent::Line { from, to } => {
                mix32(&mut h, 1);
                mixp(&mut h, from);
                mixp(&mut h, to);
            }
            PathEvent::Quadratic { from, ctrl, to } => {
                mix32(&mut h, 2);
                mixp(&mut h, from);
                mixp(&mut h, ctrl);
                mixp(&mut h, to);
            }
            PathEvent::Cubic {
                from,
                ctrl1,
                ctrl2,
                to,
            } => {
                mix32(&mut h, 3);
                mixp(&mut h, from);
                mixp(&mut h, ctrl1);
                mixp(&mut h, ctrl2);
                mixp(&mut h, to);
            }
            PathEvent::End { last, first, close } => {
                mix32(&mut h, if close { 5 } else { 4 });
                mixp(&mut h, last);
                mixp(&mut h, first);
            }
        }
    }
    h
}

fn count_sub(path: &Path) -> usize {
    path.iter()
        .filter(|e| matches!(e, PathEvent::Begin { .. }))
        .count()
}

/// 三次+二次贝塞尔+SVG 端点弧+自交 S 曲线的闭合路径（svg_builder 面）。
fn path_complex() -> Path {
    let mut b = Path::svg_builder();
    b.move_to(point(10.0, 30.0));
    b.line_to(point(70.0, 12.0));
    b.cubic_bezier_to(point(95.0, 0.0), point(125.0, 22.0), point(100.0, 50.0));
    b.quadratic_bezier_to(point(82.0, 78.0), point(50.0, 60.0));
    b.arc_to(
        vector(28.0, 18.0),
        Angle::degrees(25.0),
        ArcFlags {
            large_arc: true,
            sweep: false,
        },
        point(28.0, 44.0),
    );
    // 自交：一条横跨已有线段的 S 形三次曲线
    b.cubic_bezier_to(point(-25.0, 95.0), point(140.0, 95.0), point(18.0, 20.0));
    b.close();
    b.build()
}

/// 领结四边形（必自交于 (20,20)）+ 大环三次曲线闭合自交。
fn path_selfint() -> Path {
    let mut b = Path::builder();
    b.begin(point(0.0, 0.0));
    b.line_to(point(40.0, 40.0));
    b.line_to(point(40.0, 0.0));
    b.line_to(point(0.0, 40.0));
    b.end(true);
    b.begin(point(60.0, 10.0));
    b.cubic_bezier_to(point(150.0, 100.0), point(-30.0, 100.0), point(120.0, 10.0));
    b.end(true);
    b.build()
}

/// 外环 + 反向内环（NonZero 成孔）+ 与内环部分叠压的同向第三环。
fn path_holed() -> Path {
    let mut b = Path::builder();
    b.begin(point(0.0, 0.0));
    b.line_to(point(60.0, 0.0));
    b.line_to(point(60.0, 60.0));
    b.line_to(point(0.0, 60.0));
    b.end(true);
    b.begin(point(15.0, 15.0));
    b.line_to(point(15.0, 45.0));
    b.line_to(point(45.0, 45.0));
    b.line_to(point(45.0, 15.0));
    b.end(true);
    b.begin(point(30.0, 30.0));
    b.line_to(point(75.0, 30.0));
    b.line_to(point(75.0, 45.0));
    b.line_to(point(30.0, 45.0));
    b.end(true);
    b.build()
}

/// 开放折线 + 二次贝塞尔：stroke width/cap/dash 主体。
fn path_open() -> Path {
    let mut b = Path::builder();
    b.begin(point(0.0, 0.0));
    b.line_to(point(40.0, 0.0));
    b.line_to(point(40.0, 30.0));
    b.quadratic_bezier_to(point(40.0, 55.0), point(10.0, 55.0));
    b.line_to(point(10.0, 20.0));
    b.end(false);
    b.build()
}

/// ≈3.8° 浅转角：miter 长度过限，四种 join 回退路径全不同。
fn path_sharp() -> Path {
    let mut b = Path::builder();
    b.begin(point(0.0, 10.0));
    b.line_to(point(30.0, 10.0));
    b.line_to(point(60.0, 12.0));
    b.line_to(point(90.0, 12.0));
    b.end(false);
    b.build()
}

/// 孤立点零长子路径（stroke 空 cap 路径：Butt 空/Square 方/Round 圆）。
fn path_point(x: f32, y: f32) -> Path {
    let mut b = Path::builder();
    b.begin(point(x, y));
    b.end(false);
    b.build()
}

/// 零长线段串（退化边）。
fn path_zerolen() -> Path {
    let mut b = Path::builder();
    b.begin(point(20.0, 20.0));
    b.line_to(point(20.0, 20.0));
    b.line_to(point(20.0, 20.0));
    b.end(false);
    b.build()
}

/// 全重合点的闭合"三角形"。
fn path_coincident() -> Path {
    let mut b = Path::builder();
    b.begin(point(7.0, 7.0));
    b.line_to(point(7.0, 7.0));
    b.line_to(point(7.0, 7.0));
    b.end(true);
    b.build()
}

/// 自定义属性路径：attribute[0] = 每端点线宽因子（variable_line_width）。
fn path_varwidth() -> Path {
    let mut b = Path::builder_with_attributes(1);
    b.begin(point(0.0, 0.0), &[1.0]);
    b.line_to(point(50.0, 0.0), &[0.25]);
    b.line_to(point(50.0, 40.0), &[2.0]);
    b.quadratic_bezier_to(point(50.0, 70.0), point(0.0, 70.0), &[1.0]);
    b.end(false);
    b.build()
}

/// walk_along_path + RepeatedPattern 手切虚线：偶数段落笔 begin、奇数段
/// 抬笔 line_to+end，生成只含直线段的子路径集合。
fn dash_path(path: &Path, pattern: &[f32], start: f32, tolerance: f32) -> Path {
    let mut builder = Path::builder();
    let mut open = false;
    let mut idx = 0u32;
    {
        let mut pat = RepeatedPattern {
            callback: &mut |event: WalkerEvent| {
                if idx % 2 == 0 {
                    builder.begin(event.position);
                    open = true;
                } else {
                    builder.line_to(event.position);
                    builder.end(false);
                    open = false;
                }
                idx += 1;
                true
            },
            intervals: pattern,
            index: 0,
        };
        walk_along_path(path.iter(), start, tolerance, &mut pat);
    }
    if open {
        builder.end(false);
    }
    builder.build()
}

fn main() {
    let mut ft = FillTessellator::new();
    let mut st = StrokeTessellator::new();

    let complex = path_complex();
    let selfint = path_selfint();
    let holed = path_holed();
    let open = path_open();
    let sharp = path_sharp();

    for (name, p) in [
        ("complex", &complex),
        ("selfint", &selfint),
        ("holed", &holed),
        ("open", &open),
        ("sharp", &sharp),
    ] {
        println!("path/{name} ne={} pfnv={:016x}", p.iter().count(), path_fnv(p));
    }

    // ---- Fill：规则 × 路径矩阵 ----
    let fo = FillOptions::default();
    fill(
        &mut ft,
        "fill/complex/evenodd",
        &complex,
        &fo.with_fill_rule(FillRule::EvenOdd),
    );
    fill(
        &mut ft,
        "fill/complex/nonzero",
        &complex,
        &fo.with_fill_rule(FillRule::NonZero),
    );
    fill(&mut ft, "fill/complex/t0.01", &complex, &fo.with_tolerance(0.01));
    fill(
        &mut ft,
        "fill/complex/horiz",
        &complex,
        &fo.with_sweep_orientation(Orientation::Horizontal),
    );
    fill(&mut ft, "fill/selfint/evenodd", &selfint, &fo);
    fill(
        &mut ft,
        "fill/selfint/nonzero",
        &selfint,
        &fo.with_fill_rule(FillRule::NonZero),
    );
    // 关相交检查作用于自交路径：Ok（错误几何）或 Err（ErrorCode(1)）皆确定。
    fill(
        &mut ft,
        "fill/selfint/no-xcheck",
        &selfint,
        &fo.with_intersections(false),
    );
    fill(&mut ft, "fill/holed/evenodd", &holed, &fo);
    fill(
        &mut ft,
        "fill/holed/nonzero",
        &holed,
        &fo.with_fill_rule(FillRule::NonZero),
    );

    // ---- 基本形状 fast path ----
    fill_shape(&mut ft, "shape/rect", &fo, |t, o, gb| {
        t.tessellate_rectangle(
            &Box2D {
                min: point(0.0, 0.0),
                max: point(45.0, 27.0),
            },
            o,
            gb,
        )
    });
    fill_shape(&mut ft, "shape/circle", &fo, |t, o, gb| {
        t.tessellate_circle(point(30.0, 30.0), 22.5, o, gb)
    });
    fill_shape(&mut ft, "shape/ellipse", &fo, |t, o, gb| {
        t.tessellate_ellipse(
            point(0.0, 0.0),
            vector(40.0, 18.0),
            Angle::degrees(35.0),
            Winding::Positive,
            o,
            gb,
        )
    });

    // ---- Fill 退化/错误 ----
    fill(&mut ft, "fill/degen/empty", &Path::new(), &fo);
    fill(&mut ft, "fill/degen/point", &path_point(5.0, 5.0), &fo);
    fill(&mut ft, "fill/degen/zerolen", &path_zerolen(), &fo);
    fill(&mut ft, "fill/degen/coincident", &path_coincident(), &fo);
    // tolerance 错误用纯线段路径：带曲线时事件队列会先用该 tolerance 平化曲线，
    // NaN/0.0 会在 lyon_geom 平化的 debug_assert 处 panic（dev profile），轮不到
    // tessellate_impl 的参数校验返回 Err。
    fill(
        &mut ft,
        "fill/err/tol-nan",
        &holed,
        &fo.with_tolerance(f32::NAN),
    );
    fill(&mut ft, "fill/err/tol-zero", &holed, &fo.with_tolerance(0.0));
    let nan_events = [
        PathEvent::Begin {
            at: point(0.0, 0.0),
        },
        PathEvent::Line {
            from: point(0.0, 0.0),
            to: point(f32::NAN, 5.0),
        },
        PathEvent::Line {
            from: point(f32::NAN, 5.0),
            to: point(10.0, 5.0),
        },
        PathEvent::End {
            last: point(10.0, 5.0),
            first: point(0.0, 0.0),
            close: true,
        },
    ];
    fill_events(&mut ft, "fill/err/pos-nan", &nan_events, &fo);

    // ---- Stroke：width / join / cap ----
    let so = StrokeOptions::default();
    for (label, w) in [("w0.5", 0.5f32), ("w2", 2.0), ("w8", 8.0)] {
        stroke(
            &mut st,
            &format!("stroke/open/{label}"),
            &open,
            &so.with_line_width(w),
        );
    }
    for (label, join) in [
        ("miter", LineJoin::Miter),
        ("miterclip", LineJoin::MiterClip),
        ("round", LineJoin::Round),
        ("bevel", LineJoin::Bevel),
    ] {
        stroke(
            &mut st,
            &format!("stroke/sharp/{label}"),
            &sharp,
            &so.with_line_width(6.0).with_line_join(join),
        );
    }
    stroke(
        &mut st,
        "stroke/sharp/miter-lim1",
        &sharp,
        &so.with_line_width(6.0)
            .with_line_join(LineJoin::Miter)
            .with_miter_limit(1.0),
    );
    for (label, cap) in [
        ("butt", LineCap::Butt),
        ("square", LineCap::Square),
        ("round", LineCap::Round),
    ] {
        stroke(
            &mut st,
            &format!("stroke/open/cap-{label}"),
            &open,
            &so.with_line_width(3.0).with_line_cap(cap),
        );
    }
    stroke(
        &mut st,
        "stroke/open/cap-mixed",
        &open,
        &so.with_line_width(3.0)
            .with_start_cap(LineCap::Round)
            .with_end_cap(LineCap::Square),
    );

    // ---- Stroke：dash（walk+RepeatedPattern 切子路径）----
    let d1 = dash_path(&open, &[12.0, 6.0], 0.0, 0.1);
    println!("dash/d12-6 sub={} pfnv={:016x}", count_sub(&d1), path_fnv(&d1));
    stroke(&mut st, "stroke/dash/d12-6-w2", &d1, &so.with_line_width(2.0));
    let d2 = dash_path(&open, &[3.0, 5.0, 8.0, 5.0], 2.5, 0.1);
    println!(
        "dash/d3853-off2.5 sub={} pfnv={:016x}",
        count_sub(&d2),
        path_fnv(&d2)
    );
    stroke(
        &mut st,
        "stroke/dash/d3853-round",
        &d2,
        &so.with_line_width(1.5).with_line_cap(LineCap::Round),
    );

    // ---- Stroke：闭环大输出 + variable line width ----
    stroke(
        &mut st,
        "stroke/complex/closed",
        &complex,
        &so.with_line_width(1.5).with_line_join(LineJoin::Round),
    );
    let vw = path_varwidth();
    stroke(
        &mut st,
        "stroke/varwidth",
        &vw,
        &so.with_line_width(6.0).with_variable_line_width(0),
    );

    // ---- Stroke 退化 ----
    stroke(&mut st, "stroke/degen/empty", &Path::new(), &so);
    for (label, cap) in [
        ("butt", LineCap::Butt),
        ("square", LineCap::Square),
        ("round", LineCap::Round),
    ] {
        stroke(
            &mut st,
            &format!("stroke/degen/point-{label}"),
            &path_point(5.0, 5.0),
            &so.with_line_cap(cap),
        );
    }
    stroke(
        &mut st,
        "stroke/degen/zerolen",
        &path_zerolen(),
        &so.with_line_cap(LineCap::Round),
    );
}
