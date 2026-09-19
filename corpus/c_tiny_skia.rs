#!/usr/bin/env mirvm
---
[dependencies]
tiny-skia = { version = "0.11", default-features = false, features = ["std"] }
---
// tiny-skia 0.11 scalar path (default-features off: no simd/png) 2D rasterization
// differential. 32 deterministic rounds on a 128x96 Pixmap, each round covering the whole
// surface: fill_rect / hand-built rounded rect (κ cubic arc fit) / circle / closed cubic
// Bezier (EvenOdd fill + dash stroke) / three-stop linear gradient (Pad+Reflect) /
// rotate∘scale∘translate composition / Mask clip / SourceOver·Plus·Multiply alpha compositing
// / draw_pixmap of a semi-transparent rotated tile (Bilinear). Repeated rounds push hot
// functions into JIT output at MIRVM_JIT_THRESHOLD=1. Each round prints a pixel FNV-1a; at the
// end it samples RGBA hex, float to_bits, and boundary/error-path booleans. All constants
// fixed; no randomness/time/addresses; empty stderr on success.
use tiny_skia::*;

const W: u32 = 128;
const H: u32 = 96;
const ROUNDS: u32 = 32;

/// Color table cycled per round (the index derives deterministically from the round number).
const PALETTE: [[u8; 4]; 3] = [
    [200, 60, 48, 255],
    [48, 144, 200, 255],
    [220, 170, 40, 255],
];

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Hand-built rounded rect: four cubic Beziers approximate the 90° arc (κ constant), float-heavy.
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    const KAPPA: f32 = 0.5522847498307936;
    let c = r * KAPPA;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + c, y, x + w, y + r - c, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + c, x + w - r + c, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - c, y + h, x, y + h - r + c, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - c, x + r - c, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

/// One full round: six drawing stages over the whole API surface; returns the composed transform.
fn draw_scene(pm: &mut Pixmap, tile: PixmapRef<'_>, round: u32) -> Transform {
    let rot = (round * 7) as f32 * 1.5; // rotation angle (degrees)
    let dx = ((round * 13) % 11) as f32 - 5.0; // horizontal jitter
    let phase = round as f32 * 0.03125; // gradient phase (2^-5 steps, exact in binary)
    let c = PALETTE[(round as usize) % PALETTE.len()];

    pm.fill(Color::from_rgba8(24, 28, 36, 255));

    // ① fill_rect: opaque solid-color SourceOver
    let mut paint = Paint::default();
    paint.set_color_rgba8(c[0], c[1], c[2], c[3]);
    pm.fill_rect(
        Rect::from_xywh(6.0 + dx, 8.0, 40.0, 26.0).unwrap(),
        &paint,
        Transform::identity(),
        None,
    );

    // ② rounded rect + three-stop linear gradient (Pad, gradient transform rotates per round), AA on
    let rr = rounded_rect_path(52.0, 6.0, 64.0, 30.0, 8.0);
    paint.shader = LinearGradient::new(
        Point::from_xy(52.0, 6.0),
        Point::from_xy(116.0, 36.0),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(255, 128, 0, 255)),
            GradientStop::new(0.55, Color::from_rgba8(40, 200, 120, 255)),
            GradientStop::new(1.0, Color::from_rgba8(32, 64, 255, 255)),
        ],
        SpreadMode::Pad,
        Transform::from_rotate(rot),
    )
    .unwrap();
    paint.anti_alias = true;
    pm.fill_path(&rr, &paint, FillRule::Winding, Transform::identity(), None);

    // ③ circle + rotate∘scale∘translate composition transform + semi-transparent white SourceOver
    let mut pb = PathBuilder::new();
    pb.push_circle(30.0, 62.0, 18.0);
    let circle = pb.finish().unwrap();
    paint.shader = Shader::SolidColor(Color::from_rgba8(255, 255, 255, 140));
    paint.blend_mode = BlendMode::SourceOver;
    let ts = Transform::from_rotate_at(rot, 30.0, 62.0)
        .post_scale(1.35, 0.75)
        .post_translate(4.0, -2.0);
    pm.fill_path(&circle, &paint, FillRule::Winding, ts, None);

    // ④ closed cubic Bezier (with inner hole): EvenOdd fill (Reflect gradient) + dash stroke
    let mut pb = PathBuilder::new();
    pb.move_to(66.0, 44.0);
    pb.cubic_to(92.0, 40.0, 118.0, 52.0, 112.0, 70.0);
    pb.cubic_to(106.0, 88.0, 78.0, 90.0, 66.0, 78.0);
    pb.cubic_to(54.0, 66.0, 52.0, 50.0, 66.0, 44.0);
    pb.close();
    pb.move_to(84.0, 58.0);
    pb.cubic_to(92.0, 56.0, 98.0, 62.0, 94.0, 70.0);
    pb.cubic_to(90.0, 78.0, 78.0, 76.0, 76.0, 68.0);
    pb.cubic_to(74.0, 60.0, 78.0, 59.0, 84.0, 58.0);
    pb.close();
    let bez = pb.finish().unwrap();
    paint.shader = LinearGradient::new(
        Point::from_xy(64.0 + phase, 60.0),
        Point::from_xy(88.0 + phase, 60.0),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(255, 0, 128, 230)),
            GradientStop::new(1.0, Color::from_rgba8(0, 200, 255, 230)),
        ],
        SpreadMode::Reflect,
        Transform::identity(),
    )
    .unwrap();
    pm.fill_path(&bez, &paint, FillRule::EvenOdd, Transform::identity(), None);

    paint.shader = Shader::SolidColor(Color::from_rgba8(255, 220, 40, 255));
    let stroke = Stroke {
        width: 2.5,
        miter_limit: 4.0,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        dash: StrokeDash::new(vec![6.0, 3.5, 1.5, 3.5], 1.25),
    };
    pm.stroke_path(&bez, &paint, &stroke, Transform::identity(), None);

    // ⑤ clip: a Plus-composited rect inside a rounded-rect clip mask (out-of-bounds parts clipped)
    let mut mask = Mask::new(W, H).unwrap();
    let clip_shape = rounded_rect_path(8.0 + dx, 40.0, 44.0, 48.0, 10.0);
    mask.fill_path(&clip_shape, FillRule::Winding, true, Transform::identity());
    paint.shader = Shader::SolidColor(Color::from_rgba8(60, 120, 255, 200));
    paint.blend_mode = BlendMode::Plus;
    pm.fill_rect(
        Rect::from_xywh(0.0, 40.0, 60.0, 48.0).unwrap(),
        &paint,
        Transform::identity(),
        Some(&mask),
    );

    // ⑥ draw_pixmap: opacity 0.6 + Multiply + rotated Bilinear sampling
    let ppaint = PixmapPaint {
        opacity: 0.6,
        blend_mode: BlendMode::Multiply,
        quality: FilterQuality::Bilinear,
    };
    pm.draw_pixmap(88, 6, tile, &ppaint, Transform::from_rotate(rot), None);

    ts
}

fn main() {
    let mut pm = Pixmap::new(W, H).unwrap();

    // 32x32 diagonal gradient tile (draw_pixmap's source; contents unchanged across rounds)
    let mut tile = Pixmap::new(32, 32).unwrap();
    let mut tpaint = Paint::default();
    tpaint.shader = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(32.0, 32.0),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(255, 255, 255, 255)),
            GradientStop::new(1.0, Color::from_rgba8(90, 30, 160, 255)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    let tile_rect = PathBuilder::from_rect(Rect::from_xywh(0.0, 0.0, 32.0, 32.0).unwrap());
    tile.fill_path(&tile_rect, &tpaint, FillRule::Winding, Transform::identity(), None);
    println!("tile fnv={:016x}", fnv1a(tile.data()));

    let mut ts = Transform::identity();
    for round in 0..ROUNDS {
        ts = draw_scene(&mut pm, tile.as_ref(), round);
        println!("r{round:02} fnv={:016x}", fnv1a(pm.data()));
    }

    // bit-locked float-output printing (last round's transform)
    let mut pt = Point::from_xy(30.0, 62.0);
    ts.map_point(&mut pt);
    println!("map-bits x={:08x} y={:08x}", pt.x.to_bits(), pt.y.to_bits());
    let (sx, sy) = ts.get_scale();
    println!("scale-bits sx={:08x} sy={:08x}", sx.to_bits(), sy.to_bits());
    println!("invert-some={}", ts.invert().is_some());

    // boundary/error paths (textually deterministic booleans)
    println!("pixmap-0-size-none={}", Pixmap::new(0, 10).is_none());
    println!("rect-nan-none={}", Rect::from_xywh(f32::NAN, 0.0, 1.0, 1.0).is_none());
    println!("color-oob-none={}", Color::from_rgba(1.5, 0.0, 0.0, 1.0).is_none());
    println!(
        "premul-bad-none={}",
        PremultipliedColorU8::from_rgba(200, 0, 0, 100).is_none()
    );
    println!("dash-empty-none={}", StrokeDash::new(vec![], 0.0).is_none());
    let empty_grad = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(1.0, 1.0),
        vec![],
        SpreadMode::Pad,
        Transform::identity(),
    );
    println!("grad-empty-none={}", empty_grad.is_none());
    let solid_grad = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(1.0, 1.0),
        vec![GradientStop::new(0.5, Color::from_rgba8(1, 2, 3, 255))],
        SpreadMode::Pad,
        Transform::identity(),
    );
    println!("grad-single-solid={}", matches!(solid_grad, Some(Shader::SolidColor(_))));
    println!("grad-degenerate-pad-solid={}", {
        let g = LinearGradient::new(
            Point::from_xy(5.0, 5.0),
            Point::from_xy(5.0, 5.0),
            vec![
                GradientStop::new(0.0, Color::from_rgba8(9, 9, 9, 255)),
                GradientStop::new(1.0, Color::from_rgba8(8, 8, 8, 255)),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        );
        matches!(g, Some(Shader::SolidColor(_)))
    });
    println!(
        "scale0-invert-none={}",
        Transform::from_scale(0.0, 1.0).invert().is_none()
    );

    // sampled pixel RGBA hex + final hash + opaque pixel count (last round's scene)
    let pts = [
        (0, 0),
        (7, 9),
        (25, 20),
        (53, 7),
        (80, 20),
        (110, 33),
        (30, 62),
        (84, 66),
        (100, 75),
        (20, 60),
        (95, 12),
        (120, 40),
        (64, 48),
        (127, 95),
    ];
    for (x, y) in pts {
        let p = pm.pixel(x, y).unwrap();
        println!(
            "px({x},{y})={:02x}{:02x}{:02x}{:02x}",
            p.red(),
            p.green(),
            p.blue(),
            p.alpha()
        );
    }
    let opaque = pm.pixels().iter().filter(|p| p.is_opaque()).count();
    println!("opaque={} total={}", opaque, pm.pixels().len());
    println!("final fnv={:016x}", fnv1a(pm.data()));
}
