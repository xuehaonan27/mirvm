#!/usr/bin/env mirvm
---
[dependencies]
lyon = "1"
---
// lyon 1.0 2D tessellation differential over fixed paths and floating point.
// Fixed path set: complex (cubic + quadratic bezier + SVG endpoint arc via arc_to
// + a self-intersecting S curve, through svg_builder), selfint (bowtie quad plus a
// closed self-intersecting large cubic), holed (outer ring + reverse inner ring
// for a hole + an overlapping third ring, distinguishing EvenOdd/NonZero), open
// (open polyline + quadratic bezier, the stroke workhorse) and sharp (~3.8 degrees,
// the miter limit fallback matrix).
// Fill: EvenOdd/NonZero x complex/selfint/holed; tolerance 0.1/0.01; Vertical and
// Horizontal sweep; handle_intersections=false on self-intersecting paths; and the
// basic-shape fast path tessellate_rectangle/circle/ellipse.
// Stroke: width 0.5/2/8; join Miter/MiterClip/Round/Bevel with miter_limit 1.0;
// cap Butt/Square/Round plus mismatched first/last caps; two dash cases (lyon 1.x
// StrokeOptions has no built-in dash, so walk_along_path + RepeatedPattern cut the
// dashed subpaths before stroking); a variable_line_width custom-attribute path;
// and a closed complex path with Round joins for a large-output fnv anchor.
// Deterministic: all constants fixed; f32 always as to_bits; no HashMap/time/thread.
//
//
//
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

/// Vertex abstraction: mixes every field into FNV-1a and yields coordinates for per-vertex bit dumps.
trait Out {
    fn mix(&self, h: &mut u64);
    fn xy(&self) -> (f32, f32);
}

/// Fill vertex: position + as_endpoint_id (u32::MAX at self-intersections or curve interiors).
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

/// Stroke vertex: position + normal + advancement + actual line width + side.
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

/// Signed triangle area sum in index order (f32 coords accumulated in f64).
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

/// Feeds a raw event stream straight in, bypassing the Path builder's debug_assert
/// NaN check so the tessellator's own validation error path is exercised.
fn fill_events(t: &mut FillTessellator, label: &str, events: &[PathEvent], options: &FillOptions) {
    let mut buffers: VertexBuffers<FVert, u32> = VertexBuffers::new();
    let r = {
        let mut gb = BuffersBuilder::new(&mut buffers, FCtor);
        t.tessellate(events.iter().copied(), options, &mut gb)
    };
    out(label, r, &buffers);
}

/// Basic-shape fast path (rectangle/circle/ellipse skip the Path data structure).
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

/// Fingerprint of the event stream in output order (tag + every coordinate's bits).
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

/// Closed path with cubic + quadratic beziers, an SVG endpoint arc and a self-intersecting S curve.
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
    // self-intersection: an S-shaped cubic crossing an existing segment
    b.cubic_bezier_to(point(-25.0, 95.0), point(140.0, 95.0), point(18.0, 20.0));
    b.close();
    b.build()
}

/// Bowtie quad (always self-intersecting at (20,20)) + a closed self-intersecting large cubic.
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

/// Outer ring + reverse inner ring (a NonZero hole) + a same-winding third ring overlapping it.
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

/// Open polyline + quadratic bezier: the body of the stroke width/cap/dash cases.
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

/// ~3.8 degree shallow corner: the miter length exceeds the limit, so all four joins differ.
fn path_sharp() -> Path {
    let mut b = Path::builder();
    b.begin(point(0.0, 10.0));
    b.line_to(point(30.0, 10.0));
    b.line_to(point(60.0, 12.0));
    b.line_to(point(90.0, 12.0));
    b.end(false);
    b.build()
}

/// Isolated-point zero-length subpath (stroke caps: Butt empty, Square a square, Round a circle).
fn path_point(x: f32, y: f32) -> Path {
    let mut b = Path::builder();
    b.begin(point(x, y));
    b.end(false);
    b.build()
}

/// A run of zero-length segments (degenerate edges).
fn path_zerolen() -> Path {
    let mut b = Path::builder();
    b.begin(point(20.0, 20.0));
    b.line_to(point(20.0, 20.0));
    b.line_to(point(20.0, 20.0));
    b.end(false);
    b.build()
}

/// A closed "triangle" whose points all coincide.
fn path_coincident() -> Path {
    let mut b = Path::builder();
    b.begin(point(7.0, 7.0));
    b.line_to(point(7.0, 7.0));
    b.line_to(point(7.0, 7.0));
    b.end(true);
    b.build()
}

/// Custom-attribute path: attribute[0] is the per-endpoint line width factor.
fn path_varwidth() -> Path {
    let mut b = Path::builder_with_attributes(1);
    b.begin(point(0.0, 0.0), &[1.0]);
    b.line_to(point(50.0, 0.0), &[0.25]);
    b.line_to(point(50.0, 40.0), &[2.0]);
    b.quadratic_bezier_to(point(50.0, 70.0), point(0.0, 70.0), &[1.0]);
    b.end(false);
    b.build()
}

/// walk_along_path + RepeatedPattern dash cutting: even segments begin a stroke,
/// odd segments lift the pen with line_to+end, yielding straight-segment subpaths.
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

    // ---- Fill: fill rule x path matrix ----
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
    // intersections off on a self-intersecting path: Ok (bad geometry) or Err, both fixed.
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

    // ---- basic-shape fast path ----
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

    // ---- Fill degenerate/error ----
    fill(&mut ft, "fill/degen/empty", &Path::new(), &fo);
    fill(&mut ft, "fill/degen/point", &path_point(5.0, 5.0), &fo);
    fill(&mut ft, "fill/degen/zerolen", &path_zerolen(), &fo);
    fill(&mut ft, "fill/degen/coincident", &path_coincident(), &fo);
    // Tolerance errors use a straight-segment path: with curves present the event
    // queue flattens them first, so NaN/0.0 would panic at lyon_geom's flatten
    // debug_assert (dev profile) before tessellate_impl could return Err.
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

    // ---- Stroke: width / join / cap ----
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

    // ---- Stroke: dash (walk + RepeatedPattern subpath cutting) ----
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

    // ---- Stroke: closed-path large output + variable line width ----
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

    // ---- Stroke degenerate ----
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
