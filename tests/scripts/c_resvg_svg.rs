#!/usr/bin/env mirvm
---
[dependencies]
# Pinned to =0.47.0 (crates.io max stable as searched on 2026-07-18; resvg/usvg/tiny-skia
# share the linebender/resvg 0.47 release train). default-features=false +
# features=["text"]: text pulls fontdb 0.23.0 / rustybuzz 0.20.1 / ttf-parser
# 0.25.1(gvar-alloc) / unicode-bidi / unicode-script / unicode-vo;
# system-fonts and memmap-fonts stay off, so no system font directory is scanned and no font file is mmapped.
usvg = { version = "=0.47.0", default-features = false, features = ["text"] }
# tiny-skia scalar backend (paired with usvg's tiny-skia-path 0.12.0 dependency edge).
# Bypass note (semantics unchanged; a recheck of the same fallback at 0.47):
# resvg 0.47 depends on `tiny-skia = "0.12.0"` with the dep edge carrying default features (simd
# on), and its f32x4/f32x8 raster pipeline reaches external LLVM intrinsics under this nightly
# core_arch (`_mm_max_ps` -> the `llvm.x86.sse.max.ps` family) that mirvm does not build, so it
# TRAPs; the resvg-0.47.0/Cargo.toml edge was rechecked and still does not set
# default-features=false. Cargo features cannot be subtracted downstream, so the resvg ->
# tiny-skia edge cannot be turned off from below. The driver therefore depends on usvg plus a
# direct tiny-skia with the scalar backend (the c_tiny_skia route, green in all three
# dimensions). Rendering semantics come from the mini renderer below, ported line by
# line from resvg 0.47's src/{render,path,clip,geom}.rs (including the render.rs dispatch
# Node::Text(text) => render_group(text.flattened()): usvg has already shaped, laid out and
# outlined the glyphs, so flattened is a plain tree). filters/masks/images/patterns are absent here.
tiny-skia = { version = "=0.12.0", default-features = false, features = ["std"] }
# fontdb deterministic font source: 17 include_bytes! embedded fonts (Libertinus Serif x6 /
# NewCM Math x3 / NewCM10 x4 / DejaVu Sans Mono x4), never touching system fonts -- the same
# font-source approach as c_typst_pdf.
typst-assets = { version = "=0.15.1", features = ["fonts"] }
---
// c_resvg_svg -- a full SVG rendering differential for resvg/usvg 0.47: paths, gradients and
// text (embedded fontdb fonts) rasterize to pixel FNV on top of the already-passing
// tiny-skia/fontdue layer. The dependency lines are all pinned on 0.47 and the text surface
// covers shaping/layout/decoration/textPath/bidi/fallback.
//
// [Status: expected-red under the C (JIT) dimension] A (mirvm default) and B (native) are both
// green with 141 stdout lines byte-identical, empty stderr (0 bytes) and exit 0, so the driver's
// determinism is confirmed by A==B. C (MIRVM_JIT_THRESHOLD=1) panics while rendering the
// text-style document's def configuration (exit 101); the diagnostic chain follows.
// Cause (a JIT miscompile, judged from the supply side): JIT-generated code produces a value
// that diverges from the interpreter/native on the hairline-stroke rendering path, pushing
// tiny-skia's fixed-point slope out of range and tripping an assertion:
//   tiny-skia-0.12.0/src/scan/hairline_aa.rs:473:13
//   assertion failed: slope <= fdot16::ONE && slope >= -fdot16::ONE
// The "mostly horizontal" branch at line 381 is the same assertion family with the operands
// reversed; the minimal repro's hot site moves to 381 while the full driver sits stably at 473
// on both cold and warm runs.
// Evidence chain:
//   1) the interpreter output matches native bit for bit, including identical pixel FNV for the
//      same hairline-stroked text (A==B over 141 lines), so neither the input data nor the
//      algorithm diverges;
//   2) fast_div is pure integer arithmetic (left_shift(a,16)/b), so the assertion can only go
//      out of range if the input to the f32->fdot6 fixed-point conversion chain diverges under
//      JIT (|slope|<=1 follows mathematically from the branch conditions |dx|>=|dy|/|dy|>|dx|,
//      and neither the native dev profile with assertions on nor the interpreter trips it);
//   3) whether it trips drifts with the set and order of JIT-compiled functions: in the
//      minimization t6 (three tspans on the first line) trips while t7 (t6 plus one unrelated
//      line) does not, and a single glyph 'o' at the same glyph and coordinates does not --
//      suggesting a compile-order-sensitive last-ulp/aggregate difference amplified at the
//      fixed-point conversion boundary into +/-1 fdot6.
// Trigger chain: the first line of text-style, Libertinus Serif Bold 24px, stroke-width=0.6
//   (hairline stroker -> hairline AA fill), at the 9th glyph 'o' of the string "GradItaBo"
//   (x~108.688). Minimal repro /tmp/repro_resvg_svg.rs (23 variants over 8 rounds of
//   bisection; its native/interpreter fnv=417be013020ecd27 agree while JIT=1 exits 101).
//   red_code=101 (the Rust panic exit code); red_pattern matches `panicked at`
//   .*tiny-skia-0.12.0/src/scan/hairline_aa.rs: plus `assertion failed: slope` (the site
//   number drifts between 381 and 473 with the JIT cache state, same family).
// C-dimension scene: stdout stops at the "doc text-style size ..." line (the def render panics,
// 95 lines); both runs are byte-stable on stdout, stderr carries 4 panic lines and exit=101.
//
// Version pins (compatibility evidence): see the frontmatter comments. All three direct
// dependencies are pinned exactly; usvg 0.47.0 itself locks transitive constraints on fontdb
// "0.23.0", rustybuzz "0.20.1", ttf-parser "0.25.1", tiny-skia-path "0.12.0" and kurbo
// "0.13.0", matching the direct tiny-skia 0.12.0 (path 0.12.0) with no cross-line mismatch.
// rustybuzz's wasmi (wasm-shaper) is optional and off by default.
//
// Determinism:
//   * font bytes: typst_assets::fonts() yields 17 fixed assets, each anchored by per-file
//     len+FNV-1a; load order is array order (fontdb faces are a Vec in insertion order, and the
//     face dump is anchored line by line).
//   * shaping/layout: rustybuzz 0.20.1 (default features=["std"]) is a pure function of fixed
//     font bytes plus fixed strings; unicode-bidi reordering and ttf-parser/kurbo outlines are
//     all bit-deterministic; no OS randomness, wall clock, time zone or network.
//   * fallback chain: the text-fallback document asks for a family that does not exist, so usvg's
//     fixed fallback applies (Options' default font_family="Times New Roman" is absent from the
//     DB, hence fontdb's generic fallback) and the library is fixed, so the result is; the
//     nofonts document has an empty fontdb, so the whole text element is dropped (anchored by
//     found=false/children).
//   * two facts observed natively are themselves the anchors (all three dimensions run the same
//     code, so the value is the anchor and semantic correctness is not required): (1) text-deco's
//     roundtrip eq=false -- the usvg 0.47 writer does not preserve text-decoration (the
//     decoration is lost in to_string and re-parsing renders differently); (2) the six documents
//     text-basic/style/transform/bidi/deco/fallback have def fnv == crisp fnv -- glyph AA is
//     decided by text-rendering (Options.shape_rendering does not affect glyph paths) and the
//     background rect is pixel-aligned (an AA invariant); text-path has shape strokes, so
//     def != crisp and it joins the nine shape documents in proving the crisp config is real.
//   * rendering: the tiny-skia scalar backend is bit-exact IEEE; each document's whole image is
//     FNV-1a plus 8 fixed-coordinate sample pixels as RGBA hex; floats always print to_bits;
//     probes go through Vec/slices only, so no HashMap iteration order escapes.
//   * usvg/tiny-skia/rustybuzz log warnings through the log crate with no subscriber, so stderr
//     stays empty.
//
// Coverage:
//   shape surface (inherited, ported to 0.47): prim (linear+radial gradients, userSpaceOnUse/
//   three stops/stop-opacity/reflect spread, rounded rect, an evenodd self-intersecting bezier,
//   a dash stroke with an odd segment count, a transform group, an opacity group, a clipPath
//   group, a mix-blend-mode group, visibility=hidden, fill=none), vb50 (viewBox at 0.5 scale,
//   polygon, objectBoundingBox gradient), par x4 (a tall viewBox with xMidYMid
//   meet/slice/none/xMinYMax), novh (no width/height, viewBox only), crisp-attr (element-level
//   shape-rendering=crispEdges/optimizeSpeed), use-style (use x/y/opacity, style presentation
//   attributes, polyline), empty-svg (the 100% default size) and gzsvg (gzip from_data).
//   text surface (all with embedded fonts): text-basic (start/middle/end anchors, the kerning
//   string "AV To Kern", text-rendering=optimizeSpeed, a serif and a mono family), text-style
//   (tspan gradient fill/italic/bold+stroke, letter/word-spacing, textLength spacingAndGlyphs),
//   text-transform (dx/dy/rotate per-character arrays, group rotate, small-caps, baseline-shift
//   super/sub), text-path (textPath along a curve with startOffset), text-bidi (direction=rtl
//   Hebrew reordering, writing-mode=tb vertical text with a missing-glyph CJK probe), text-deco
//   (underline/overline/line-through decoration paths) and text-fallback (a fallback chain for a
//   missing family), nofonts (an empty fontdb, text elements dropped).
//   probes: font-file byte anchors x17, a full fontdb faces dump, tree-API bit-level values
//   (bbox/abs_transform bits of the p1 path and tp1 text, chunks/spans/layouted glyph id+text+
//   font id, flattened child count, tree.fontdb face count) and six error paths (bad XML,
//   width=0, empty attributes, bad gzip, non-UTF-8, non-svg root).
//   Each document renders in three configurations at 160x120: default (AA on) / no AA
//   (Options::shape_rendering=CrispEdges) / to_string re-parse roundtrip (render FNV compared
//   against the default, where the eq value itself is the anchor).
// Re-run commands (three dimensions):
//   A: target/release/mirvm run tests/scripts/c_resvg_svg.rs
//   B: d=$(grep -l 'name = "c_resvg_svg"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && cargo +nightly-2026-07-02 run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/scripts/c_resvg_svg.rs
// Measured on the same machine (2026-07-18):
//   A (mirvm default): exit 0, 141 stdout lines, 0 stderr bytes, real ~36s (deps cache warm).
//   B (cargo +nightly-2026-07-02 run -q in the script dir): exit 0, 141 stdout lines, 0 stderr
//     bytes, real ~8.5s; A==B byte-identical.
//   Sampled anchors: faces len=17; prim def fnv=7412fbca454bcd60; text-basic def
//     fnv=ce49adf0e33fcbc0; probe tp1 layouted spans=3 glyphs=11 (g0 id=40 'G'
//     font=LibertinusSerif-Regular); text-deco rt eq=false (the usvg writer drops
//     text-decoration); nofonts found=false children=1.
//   C (MIRVM_JIT_THRESHOLD=1): exit 101, stdout's 95 lines stop at text-style def, both runs
//     byte-stable, real ~13s.
//   Dependency closure 49 crates (usvg, rustybuzz, fontdb, ttf-parser, unicode-*, kurbo,
//   tiny-skia, typst-assets; the real count comes from the script dir's Cargo.lock).
use tiny_skia::{
    BlendMode, Color, FilterQuality, IntRect, Mask, MaskType, Paint, Pixmap, PixmapMut,
    PixmapPaint, Shader, SpreadMode, Transform,
};
use usvg::{Node, PaintOrder};

const W: u32 = 160;
const H: u32 = 120;

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Fixed Options: an empty fontdb plus the 17 fonts embedded in typst-assets (load order = array order).
/// Rebuilt for every parse (Options is not Clone); the same fixed input gives the same DB state.
fn make_opt() -> usvg::Options<'static> {
    let mut opt = usvg::Options::default();
    for f in typst_assets::fonts() {
        opt.fontdb_mut().load_font_data(f.to_vec());
    }
    opt
}

// ===== mini renderer: a port of the resvg 0.47 shapes/text subset =====

/// resvg geom::fit_to_rect, verbatim.
fn fit_to_rect(r: IntRect, bounds: IntRect) -> Option<IntRect> {
    let mut left = r.left();
    if left < bounds.left() {
        left = bounds.left();
    }
    let mut top = r.top();
    if top < bounds.top() {
        top = bounds.top();
    }
    let mut right = r.right();
    if right > bounds.right() {
        right = bounds.right();
    }
    let mut bottom = r.bottom();
    if bottom > bounds.bottom() {
        bottom = bounds.bottom();
    }
    IntRect::from_ltrb(left, top, right, bottom)
}

/// resvg::render's max_bbox (a constant derived from the top-level canvas).
fn max_bbox() -> IntRect {
    IntRect::from_xywh(-(W as i32) * 2, -(H as i32) * 2, W * 5, H * 5).unwrap()
}

fn render_tree(tree: &usvg::Tree, ts: Transform, pm: &mut PixmapMut) {
    render_nodes(tree.root(), ts, pm);
}

fn render_nodes(parent: &usvg::Group, ts: Transform, pm: &mut PixmapMut) {
    for node in parent.children() {
        render_node(node, ts, pm);
    }
}

/// resvg 0.47 render::render_node (the Image arm is skipped: this document set has no raster).
fn render_node(node: &Node, ts: Transform, pm: &mut PixmapMut) {
    match node {
        Node::Group(group) => {
            render_group(group, ts, pm);
        }
        Node::Path(path) => render_path(path, ts, pm),
        Node::Text(text) => {
            render_group(text.flattened(), ts, pm);
        }
        Node::Image(_) => {}
    }
}

/// The always-empty filters/mask branch of resvg 0.47 render::render_group.
fn render_group(group: &usvg::Group, ts: Transform, pm: &mut PixmapMut) -> Option<()> {
    let ts = ts.pre_concat(group.transform());
    if !group.should_isolate() {
        render_nodes(group, ts, pm);
        return Some(());
    }
    let bbox = group.layer_bounding_box().transform(ts)?;
    // filters are always empty -> the expand-by-2px branch + fit_to_rect (as in resvg)
    let ibbox = IntRect::from_xywh(
        (bbox.x().floor() as i32).checked_sub(2)?,
        (bbox.y().floor() as i32).checked_sub(2)?,
        (bbox.width().ceil() as u32).checked_add(4)?,
        (bbox.height().ceil() as u32).checked_add(4)?,
    )?;
    let ibbox = fit_to_rect(ibbox, max_bbox())?;
    let shift_ts = {
        let mut dx = bbox.x();
        let mut dy = bbox.y();
        dx -= bbox.x() - ibbox.x() as f32;
        dy -= bbox.y() - ibbox.y() as f32;
        Transform::from_translate(-dx, -dy)
    };
    let ts = shift_ts.pre_concat(ts);
    let mut sub = Pixmap::new(ibbox.width(), ibbox.height())?;
    render_nodes(group, ts, &mut sub.as_mut());
    // filters are always empty -> no filter is applied; mask is always None -> skipped
    if let Some(clip_path) = group.clip_path() {
        clip_apply(clip_path, ts, &mut sub);
    }
    let paint = PixmapPaint {
        opacity: group.opacity().get(),
        blend_mode: convert_blend_mode(group.blend_mode()),
        quality: FilterQuality::Nearest,
    };
    pm.draw_pixmap(
        ibbox.x(),
        ibbox.y(),
        sub.as_ref(),
        &paint,
        Transform::identity(),
        None,
    );
    Some(())
}

/// resvg 0.47 render::convert_blend_mode, verbatim (all 16 arms mapped).
fn convert_blend_mode(mode: usvg::BlendMode) -> BlendMode {
    match mode {
        usvg::BlendMode::Normal => BlendMode::SourceOver,
        usvg::BlendMode::Multiply => BlendMode::Multiply,
        usvg::BlendMode::Screen => BlendMode::Screen,
        usvg::BlendMode::Overlay => BlendMode::Overlay,
        usvg::BlendMode::Darken => BlendMode::Darken,
        usvg::BlendMode::Lighten => BlendMode::Lighten,
        usvg::BlendMode::ColorDodge => BlendMode::ColorDodge,
        usvg::BlendMode::ColorBurn => BlendMode::ColorBurn,
        usvg::BlendMode::HardLight => BlendMode::HardLight,
        usvg::BlendMode::SoftLight => BlendMode::SoftLight,
        usvg::BlendMode::Difference => BlendMode::Difference,
        usvg::BlendMode::Exclusion => BlendMode::Exclusion,
        usvg::BlendMode::Hue => BlendMode::Hue,
        usvg::BlendMode::Saturation => BlendMode::Saturation,
        usvg::BlendMode::Color => BlendMode::Color,
        usvg::BlendMode::Luminosity => BlendMode::Luminosity,
    }
}

/// The paint_order dispatch of resvg 0.47 path::render.
fn render_path(path: &usvg::Path, ts: Transform, pm: &mut PixmapMut) {
    if !path.is_visible() {
        return;
    }
    if path.paint_order() == PaintOrder::FillAndStroke {
        fill_path(path, BlendMode::SourceOver, ts, pm);
        stroke_path(path, ts, pm);
    } else {
        stroke_path(path, ts, pm);
        fill_path(path, BlendMode::SourceOver, ts, pm);
    }
}

/// The shapes subset of resvg 0.47 path::fill_path (the pattern arm is simplified to a skip).
fn fill_path(path: &usvg::Path, mode: BlendMode, ts: Transform, pm: &mut PixmapMut) -> Option<()> {
    let fill = path.fill()?;
    // Horizontal/vertical lines cannot be filled (an early return as in resvg)
    if path.data().bounds().width() == 0.0 || path.data().bounds().height() == 0.0 {
        return None;
    }
    let rule = match fill.rule() {
        usvg::FillRule::NonZero => tiny_skia::FillRule::Winding,
        usvg::FillRule::EvenOdd => tiny_skia::FillRule::EvenOdd,
    };
    let mut paint = Paint::default();
    match fill.paint() {
        usvg::Paint::Color(c) => {
            paint.set_color_rgba8(c.red, c.green, c.blue, fill.opacity().to_u8());
        }
        usvg::Paint::LinearGradient(lg) => {
            paint.shader = convert_linear_gradient(lg, fill.opacity())?;
        }
        usvg::Paint::RadialGradient(rg) => {
            paint.shader = convert_radial_gradient(rg, fill.opacity())?;
        }
        usvg::Paint::Pattern(_) => return None, // this document set contains no pattern
    }
    paint.anti_alias = path.rendering_mode().use_shape_antialiasing();
    paint.blend_mode = mode;
    pm.fill_path(path.data(), &paint, rule, ts, None);
    Some(())
}

/// The shapes subset of resvg 0.47 path::stroke_path.
fn stroke_path(path: &usvg::Path, ts: Transform, pm: &mut PixmapMut) -> Option<()> {
    let stroke = path.stroke()?;
    let mut paint = Paint::default();
    match stroke.paint() {
        usvg::Paint::Color(c) => {
            paint.set_color_rgba8(c.red, c.green, c.blue, stroke.opacity().to_u8());
        }
        usvg::Paint::LinearGradient(lg) => {
            paint.shader = convert_linear_gradient(lg, stroke.opacity())?;
        }
        usvg::Paint::RadialGradient(rg) => {
            paint.shader = convert_radial_gradient(rg, stroke.opacity())?;
        }
        usvg::Paint::Pattern(_) => return None,
    }
    paint.anti_alias = path.rendering_mode().use_shape_antialiasing();
    paint.blend_mode = BlendMode::SourceOver;
    pm.stroke_path(path.data(), &paint, &stroke.to_tiny_skia(), ts, None);
    Some(())
}

/// resvg 0.47's convert_linear_gradient / convert_radial_gradient.
fn convert_linear_gradient(lg: &usvg::LinearGradient, opacity: usvg::Opacity) -> Option<Shader<'_>> {
    let (mode, stops) = convert_base_gradient(lg, opacity);
    tiny_skia::LinearGradient::new(
        (lg.x1(), lg.y1()).into(),
        (lg.x2(), lg.y2()).into(),
        stops,
        mode,
        lg.transform(),
    )
}

fn convert_radial_gradient(rg: &usvg::RadialGradient, opacity: usvg::Opacity) -> Option<Shader<'_>> {
    let (mode, stops) = convert_base_gradient(rg, opacity);
    tiny_skia::RadialGradient::new(
        (rg.fx(), rg.fy()).into(),
        rg.fr().get(),
        (rg.cx(), rg.cy()).into(),
        rg.r().get(),
        stops,
        mode,
        rg.transform(),
    )
}

/// resvg 0.47 convert_base_gradient: stop alpha = stop.opacity x fill/stroke opacity.
fn convert_base_gradient(
    gradient: &usvg::BaseGradient,
    opacity: usvg::Opacity,
) -> (SpreadMode, Vec<tiny_skia::GradientStop>) {
    let mode = match gradient.spread_method() {
        usvg::SpreadMethod::Pad => SpreadMode::Pad,
        usvg::SpreadMethod::Reflect => SpreadMode::Reflect,
        usvg::SpreadMethod::Repeat => SpreadMode::Repeat,
    };
    let mut stops = Vec::with_capacity(gradient.stops().len());
    for stop in gradient.stops() {
        let alpha = stop.opacity() * opacity;
        stops.push(tiny_skia::GradientStop::new(
            stop.offset().get(),
            Color::from_rgba8(
                stop.color().red,
                stop.color().green,
                stop.color().blue,
                alpha.to_u8(),
            ),
        ));
    }
    (mode, stops)
}

/// resvg 0.47 clip::apply, verbatim (Clear draws the child shapes -> invert -> apply_mask).
fn clip_apply(clip: &usvg::ClipPath, ts: Transform, pm: &mut Pixmap) {
    let mut clip_pm = Pixmap::new(pm.width(), pm.height()).unwrap();
    clip_pm.fill(Color::BLACK);
    clip_draw_children(
        clip.root(),
        BlendMode::Clear,
        ts.pre_concat(clip.transform()),
        &mut clip_pm.as_mut(),
    );
    if let Some(nested) = clip.clip_path() {
        clip_apply(nested, ts, pm);
    }
    let mut mask = Mask::from_pixmap(clip_pm.as_ref(), MaskType::Alpha);
    mask.invert();
    pm.apply_mask(&mask);
}

/// resvg 0.47 clip::draw_children, verbatim (the Path / Text flattened / Group arms).
fn clip_draw_children(parent: &usvg::Group, mode: BlendMode, ts: Transform, pm: &mut PixmapMut) {
    for child in parent.children() {
        match child {
            Node::Path(path) => {
                if path.is_visible() {
                    fill_path(path, mode, ts, pm);
                }
            }
            Node::Text(text) => {
                clip_draw_children(text.flattened(), mode, ts, pm);
            }
            Node::Group(group) => {
                let ts = ts.pre_concat(group.transform());
                if let Some(clip) = group.clip_path() {
                    clip_group(group, clip, ts, pm);
                } else {
                    clip_draw_children(group, mode, ts, pm);
                }
            }
            _ => {}
        }
    }
}

/// resvg 0.47 clip::clip_group, verbatim (SourceOver draws the group -> clip -> Xor composite).
fn clip_group(children: &usvg::Group, clip: &usvg::ClipPath, ts: Transform, pm: &mut PixmapMut) -> Option<()> {
    let mut clip_pm = Pixmap::new(pm.width(), pm.height())?;
    clip_draw_children(children, BlendMode::SourceOver, ts, &mut clip_pm.as_mut());
    clip_apply(clip, ts, &mut clip_pm);
    let mut paint = PixmapPaint::default();
    paint.blend_mode = BlendMode::Xor;
    pm.draw_pixmap(
        0,
        0,
        clip_pm.as_ref(),
        &paint,
        Transform::identity(),
        None,
    );
    Some(())
}

// ===== document set A: shapes =====

const PRIM: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <defs>
    <linearGradient id="lg1" x1="10" y1="10" x2="150" y2="50" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="#ff6028"/>
      <stop offset="0.55" stop-color="#2878c8" stop-opacity="0.8"/>
      <stop offset="1" stop-color="#1428ff"/>
    </linearGradient>
    <radialGradient id="rg1" cx="110" cy="85" r="40" gradientUnits="userSpaceOnUse" spreadMethod="reflect">
      <stop offset="0" stop-color="#ffff60" stop-opacity="0.9"/>
      <stop offset="1" stop-color="#c82860" stop-opacity="0.4"/>
    </radialGradient>
    <clipPath id="cp1"><circle cx="120" cy="60" r="34"/></clipPath>
  </defs>
  <rect x="4" y="4" width="152" height="112" fill="#181c24"/>
  <rect id="r1" x="12" y="14" width="52" height="34" rx="6" fill="url(#lg1)"/>
  <circle cx="42" cy="78" r="22" fill="none" stroke="#f0dc28" stroke-width="3.5" stroke-dasharray="7 3.5 2"/>
  <path id="p1" d="M70 20 C95 8 120 30 108 52 S80 74 70 60 Z" fill="url(#lg1)" fill-rule="evenodd"/>
  <circle cx="30" cy="30" r="5" fill="#ffffff" visibility="hidden"/>
  <g transform="translate(20 -6) rotate(8 110 80) scale(1.1 0.9)">
    <path d="M96 60 l16 28 h-32 z" fill="url(#rg1)" stroke="#ffffff" stroke-width="2" stroke-linejoin="round"/>
    <rect x="86" y="66" width="48" height="30" fill="#40c878" fill-opacity="0.55" stroke="#0a3820" stroke-width="2.5" stroke-dasharray="6 4"/>
  </g>
  <g opacity="0.6">
    <circle cx="122" cy="52" r="26" fill="#ffb028"/>
    <rect x="100" y="36" width="44" height="32" fill="#2860c8"/>
  </g>
  <g clip-path="url(#cp1)">
    <rect x="86" y="26" width="72" height="70" fill="#e04470"/>
    <circle cx="130" cy="80" r="20" fill="#60ffe0"/>
  </g>
  <g style="mix-blend-mode:multiply" opacity="0.85">
    <ellipse cx="130" cy="30" rx="18" ry="12" fill="#30ffd0"/>
  </g>
  <path d="M8 108 h144" stroke="#c8c8c8" stroke-width="2" stroke-dasharray="10 5"/>
</svg>"##;

const VB50: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 320 240">
  <defs>
    <linearGradient id="lg2" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#f8f040"/>
      <stop offset="1" stop-color="#d03080" stop-opacity="0.6"/>
    </linearGradient>
  </defs>
  <rect x="8" y="8" width="304" height="224" fill="#202830"/>
  <rect x="24" y="28" width="104" height="68" rx="12" fill="url(#lg2)"/>
  <circle cx="84" cy="156" r="44" fill="none" stroke="#40c8ff" stroke-width="7" stroke-dasharray="14 7 4"/>
  <path d="M140 40 C190 16 240 60 216 104 S160 148 140 120 Z" fill="#c85078" fill-opacity="0.75"/>
  <polygon points="240,140 290,220 190,220" fill="#48c8a0" stroke="#ffffff" stroke-width="4"/>
  <g transform="translate(40 12) rotate(10 240 60)" opacity="0.7">
    <ellipse cx="240" cy="60" rx="60" ry="26" fill="#5a78ff"/>
  </g>
</svg>"##;

/// Tall content (viewBox 120x240) across the preserveAspectRatio lineage.
fn par_doc(par: &str) -> String {
    const BODY: &str = r##"<defs><linearGradient id="lgp" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#20c8a0" stop-opacity="0.4"/></linearGradient></defs><rect x="0" y="0" width="120" height="240" fill="#181c24"/><rect x="6" y="6" width="24" height="24" fill="#ff5030"/><circle cx="96" cy="216" r="18" fill="#3090ff"/><path d="M60 100 l30 60 h-60 z" fill="none" stroke="#e8e050" stroke-width="6" stroke-linejoin="round"/><rect x="30" y="100" width="60" height="40" fill="url(#lgp)" fill-opacity="0.8"/>"##;
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"160\" height=\"120\" viewBox=\"0 0 120 240\" preserveAspectRatio=\"{par}\">{BODY}</svg>"
    )
}

const NOVH: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 100">
  <rect x="0" y="0" width="200" height="100" fill="#242028"/>
  <circle cx="50" cy="50" r="34" fill="#e06838" fill-opacity="0.85"/>
  <path d="M100 70 Q140 10 180 50 T196 94 V94 H100 Z" fill="#48a0e0" fill-opacity="0.7"/>
  <rect x="120" y="20" width="60" height="36" rx="8" fill="none" stroke="#f0f048" stroke-width="4"/>
</svg>"##;

const CRISP_ATTR: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <rect x="0" y="0" width="160" height="120" fill="#14181c"/>
  <rect x="14" y="12" width="64" height="42" transform="rotate(7 46 33)" shape-rendering="crispEdges" fill="#40b0e0"/>
  <circle cx="112" cy="62" r="30" fill="#e09030" fill-opacity="0.7"/>
  <path d="M20 102 L60 82 L100 106 L140 72" stroke="#ff6090" stroke-width="4" fill="none" shape-rendering="optimizeSpeed"/>
</svg>"##;

const USE_STYLE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <defs>
    <g id="motif"><rect width="20" height="20" fill="#c84848"/><circle cx="10" cy="10" r="6" fill="#ffffff"/></g>
  </defs>
  <rect x="0" y="0" width="160" height="120" fill="#101418"/>
  <use href="#motif" x="20" y="30"/>
  <use href="#motif" x="60" y="70" opacity="0.5"/>
  <g style="fill:#3060d0;stroke:#f0e040;stroke-width:3"><rect x="100" y="20" width="40" height="30"/></g>
  <polyline points="10,108 40,96 70,112 100,98 130,110" fill="none" stroke="#50e080" stroke-width="2.5"/>
</svg>"##;

const EMPTY_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg"/>"##;

// ===== document set B: text (embedded families: Libertinus Serif / DejaVu Sans Mono) =====

const TEXT_BASIC: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <rect x="0" y="0" width="160" height="120" fill="#14181e"/>
  <text id="tb1" x="8" y="34" font-family="'Libertinus Serif'" font-size="22" fill="#f0e0c8">AV To Kern 012</text>
  <text x="152" y="60" text-anchor="end" font-family="'DejaVu Sans Mono'" font-size="13" fill="#60c8ff" text-rendering="optimizeSpeed">mono-end 42</text>
  <text x="80" y="92" text-anchor="middle" font-family="'Libertinus Serif'" font-size="15" fill="#ffb060">middle anchor</text>
</svg>"##;

const TEXT_STYLE: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <defs>
    <linearGradient id="lgt" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#ffe040"/>
      <stop offset="1" stop-color="#e04090" stop-opacity="0.7"/>
    </linearGradient>
  </defs>
  <rect x="0" y="0" width="160" height="120" fill="#181420"/>
  <text id="tp1" x="10" y="38" font-family="'Libertinus Serif'" font-size="24"><tspan fill="url(#lgt)">Grad</tspan><tspan font-style="italic" fill="#60d0ff">Ita</tspan><tspan font-weight="bold" fill="#ff8050" stroke="#401008" stroke-width="0.6">Bold</tspan></text>
  <text x="10" y="68" font-family="'Libertinus Serif'" font-size="14" letter-spacing="2.5" word-spacing="7" fill="#a0e0a0">spaced out run</text>
  <text x="10" y="98" font-family="'DejaVu Sans Mono'" font-size="13" textLength="132" lengthAdjust="spacingAndGlyphs" fill="#d0d060">textLength fit 9</text>
</svg>"##;

const TEXT_TRANSFORM: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <rect x="0" y="0" width="160" height="120" fill="#101820"/>
  <g transform="rotate(-7 80 60)">
    <text x="18" y="48" font-family="'Libertinus Serif'" font-size="19" fill="#80c0f0" dx="0 2 -1 3 0" dy="0 -5 4 -3 0" rotate="0 9 -7 5 0">Wave9</text>
  </g>
  <text x="18" y="76" font-family="'Libertinus Serif'" font-size="15" font-variant="small-caps" fill="#f0a0a0">Small Caps abc</text>
  <text x="18" y="104" font-family="'Libertinus Serif'" font-size="14" fill="#c0c0e0">Base<tspan baseline-shift="super" font-size="9">sup7</tspan><tspan baseline-shift="sub" font-size="9">sub2</tspan></text>
</svg>"##;

const TEXT_PATH: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <defs><path id="curve" d="M12 92 C44 38 116 38 150 88" fill="none"/></defs>
  <rect x="0" y="0" width="160" height="120" fill="#141214"/>
  <path d="M12 92 C44 38 116 38 150 88" fill="none" stroke="#383038" stroke-width="1"/>
  <text font-family="'Libertinus Serif'" font-size="13" fill="#ffe080"><textPath href="#curve" startOffset="10">text on a curved path</textPath></text>
</svg>"##;

const TEXT_BIDI: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <rect x="0" y="0" width="160" height="120" fill="#101418"/>
  <text x="10" y="40" font-family="'Libertinus Serif'" font-size="16" fill="#a0f0d0">abc <tspan direction="rtl">עברית</tspan> 123</text>
  <text x="146" y="8" writing-mode="tb" font-family="'DejaVu Sans Mono'" font-size="12" fill="#f0b0e0">Vert 縦7</text>
</svg>"##;

const TEXT_DECO: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <rect x="0" y="0" width="160" height="120" fill="#161219"/>
  <text x="14" y="42" font-family="'Libertinus Serif'" font-size="17" fill="#90d0f0" text-decoration="underline">underlined</text>
  <text x="14" y="76" font-family="'Libertinus Serif'" font-size="17" fill="#f0c090" text-decoration="overline line-through">over+through</text>
  <text x="14" y="104" font-family="'DejaVu Sans Mono'" font-size="12" fill="#b0e0b0" text-decoration="underline line-through">mono deco</text>
</svg>"##;

const TEXT_FALLBACK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="160" height="120" viewBox="0 0 160 120">
  <rect x="0" y="0" width="160" height="120" fill="#12161c"/>
  <text x="10" y="46" font-family="'NoSuch Family'" font-size="18" fill="#e0d0a0">fallback run 5</text>
  <text x="10" y="84" font-family="'NoSuch Family', 'DejaVu Sans Mono'" font-size="14" fill="#a0c0e0">second listed 3</text>
</svg>"##;

/// A small SVG pre-compressed with gzip (deflate, mtime=0): the from_data gunzip path.
const GZ_SVG: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 2, 3, 77, 141, 193, 14, 194, 32, 16, 68, 127, 101, 179, 158, 133,
    181, 88, 15, 6, 248, 23, 163, 20, 136, 104, 13, 108, 220, 250, 247, 162, 73, 141, 151, 201,
    204, 228, 77, 198, 182, 103, 132, 229, 86, 238, 205, 97, 98, 126, 28, 181, 22, 17, 37, 70,
    205, 53, 234, 129, 136, 116, 39, 16, 36, 95, 56, 57, 220, 29, 16, 82, 200, 49, 113, 247, 3,
    122, 91, 195, 153, 97, 233, 1, 225, 245, 213, 21, 220, 255, 129, 132, 48, 229, 82, 28, 110,
    204, 116, 162, 177, 199, 198, 117, 190, 134, 94, 4, 10, 100, 126, 197, 118, 93, 171, 17, 181,
    183, 159, 107, 255, 6, 251, 62, 160, 209, 162, 0, 0, 0,
];

/// Global sample coordinates (the same set for every document).
const SAMPLES: [(u32, u32); 8] = [
    (2, 2),
    (40, 30),
    (84, 60),
    (120, 40),
    (150, 110),
    (60, 100),
    (100, 20),
    (130, 84),
];

/// Parse -> render in two configurations -> compare the to_string roundtrip render -> sample pixels.
fn run_doc(name: &str, svg: &str) {
    let opt = make_opt();
    let tree = match usvg::Tree::from_str(svg, &opt) {
        Ok(t) => t,
        Err(e) => {
            println!("doc {name} parse-err {e:?}");
            return;
        }
    };
    let size = tree.size();
    println!(
        "doc {name} size w={:08x} h={:08x} children={} rootid='{}'",
        size.width().to_bits(),
        size.height().to_bits(),
        tree.root().children().len(),
        tree.root().id()
    );

    let mut pm = Pixmap::new(W, H).unwrap();
    let ts = Transform::from_scale(W as f32 / size.width(), H as f32 / size.height());
    render_tree(&tree, ts, &mut pm.as_mut());
    let fnv_def = fnv1a(pm.data());
    println!("doc {name} def   fnv={fnv_def:016x}");

    // No-AA configuration: Options::shape_rendering = CrispEdges (the same AA source as resvg)
    let mut opt2 = make_opt();
    opt2.shape_rendering = usvg::ShapeRendering::CrispEdges;
    let tree2 = match usvg::Tree::from_str(svg, &opt2) {
        Ok(t) => t,
        Err(e) => {
            println!("doc {name} crisp-parse-err {e:?}");
            return;
        }
    };
    let mut pm2 = Pixmap::new(W, H).unwrap();
    render_tree(&tree2, ts, &mut pm2.as_mut());
    println!("doc {name} crisp fnv={:016x}", fnv1a(pm2.data()));

    // to_string -> re-parse -> re-render, comparing render FNV against the default (the eq value is the anchor)
    let s = tree.to_string(&usvg::WriteOptions::default());
    let eq = match usvg::Tree::from_str(&s, &make_opt()) {
        Ok(t3) => {
            let mut pm3 = Pixmap::new(W, H).unwrap();
            render_tree(&t3, ts, &mut pm3.as_mut());
            fnv1a(pm3.data()) == fnv_def
        }
        Err(_) => false,
    };
    println!(
        "doc {name} rt len={} sfnv={:016x} eq={}",
        s.len(),
        fnv1a(s.as_bytes()),
        eq
    );

    print_samples(name, &pm);
}

fn print_samples(name: &str, pm: &Pixmap) {
    let mut line = format!("doc {name} px");
    for &(x, y) in &SAMPLES {
        let p = pm.pixel(x, y).unwrap();
        line.push_str(&format!(
            " {x},{y}:{:02x}{:02x}{:02x}{:02x}",
            p.red(),
            p.green(),
            p.blue(),
            p.alpha()
        ));
    }
    println!("{line}");
}

/// Bit-level tree-API probes for a text node: chunks/spans/layouted glyphs/flattened/bbox bits.
fn probe_text(id: &str, svg: &str) {
    let opt = make_opt();
    let tree = usvg::Tree::from_str(svg, &opt).unwrap();
    let Some(Node::Text(t)) = tree.node_by_id(id) else {
        println!("probe {id} missing");
        return;
    };
    let bb = t.bounding_box();
    let at = t.abs_transform();
    println!(
        "probe {id} chunks={} treedb={} bbox x={:08x} y={:08x} w={:08x} h={:08x}",
        t.chunks().len(),
        tree.fontdb().len(),
        bb.x().to_bits(),
        bb.y().to_bits(),
        bb.width().to_bits(),
        bb.height().to_bits()
    );
    println!(
        "probe {id} at sx={:08x} sy={:08x} tx={:08x} ty={:08x}",
        at.sx.to_bits(),
        at.sy.to_bits(),
        at.tx.to_bits(),
        at.ty.to_bits()
    );
    for (ci, ch) in t.chunks().iter().enumerate() {
        println!(
            "probe {id} chunk{ci} anchor={:?} spans={} text={:?}",
            ch.anchor(),
            ch.spans().len(),
            ch.text()
        );
        if let Some(sp) = ch.spans().first() {
            println!(
                "probe {id} span{ci} fam={:?} w={} style={:?} fs={:08x} ls={:08x} ws={:08x} tl={:?}",
                sp.font().families(),
                sp.font().weight(),
                sp.font().style(),
                sp.font_size().get().to_bits(),
                sp.letter_spacing().to_bits(),
                sp.word_spacing().to_bits(),
                sp.text_length().map(|v| v.to_bits())
            );
        }
    }
    let spans = t.layouted();
    let glyphs: usize = spans.iter().map(|s| s.positioned_glyphs.len()).sum();
    println!(
        "probe {id} layouted spans={} glyphs={} flat={}",
        spans.len(),
        glyphs,
        t.flattened().children().len()
    );
    for (gi, g) in spans
        .iter()
        .flat_map(|s| s.positioned_glyphs.iter())
        .take(4)
        .enumerate()
    {
        println!(
            "probe {id} g{gi} id={} text={:?} font={:?}",
            g.id.0, g.text, g.font
        );
    }
}

fn main() {
    // ---- ① font byte anchors: embedded assets (load order = array order) ----
    let mut total = 0usize;
    let mut count = 0usize;
    for (i, f) in typst_assets::fonts().enumerate() {
        println!("font[{i:02}] len={} fnv={:016x}", f.len(), fnv1a(f));
        total += f.len();
        count += 1;
    }
    println!("font total files={count} bytes={total}");

    // ---- ② fontdb surface: full faces dump (Vec insertion order) ----
    let opt0 = make_opt();
    println!("faces len={}", opt0.fontdb.len());
    for (i, face) in opt0.fontdb.faces().enumerate() {
        println!(
            "face[{i:02}] id={:?} fam={:?} ps={:?} style={:?} weight={:?} stretch={:?} mono={}",
            face.id,
            face.families,
            face.post_script_name,
            face.style,
            face.weight,
            face.stretch,
            face.monospaced
        );
    }

    // ---- ③ shape surface ----
    run_doc("prim", PRIM);

    // bit-level tree-API probes for prim: id lookup / abs_transform / abs_bounding_box
    let opt = make_opt();
    let tree = usvg::Tree::from_str(PRIM, &opt).unwrap();
    let p1 = tree.node_by_id("p1").unwrap();
    let bb = p1.abs_bounding_box();
    let at = p1.abs_transform();
    println!(
        "probe p1 id='{}' bb x={:08x} y={:08x} w={:08x} h={:08x}",
        p1.id(),
        bb.x().to_bits(),
        bb.y().to_bits(),
        bb.width().to_bits(),
        bb.height().to_bits()
    );
    println!(
        "probe p1 at sx={:08x} sy={:08x} tx={:08x} ty={:08x}",
        at.sx.to_bits(),
        at.sy.to_bits(),
        at.tx.to_bits(),
        at.ty.to_bits()
    );
    println!("probe missing-is-none={}", tree.node_by_id("no-such").is_none());

    run_doc("vb50", VB50);
    for (name, par) in [
        ("par-meet", "xMidYMid meet"),
        ("par-slice", "xMidYMid slice"),
        ("par-none", "none"),
        ("par-minmax", "xMinYMax meet"),
    ] {
        run_doc(name, &par_doc(par));
    }
    run_doc("novh", NOVH);
    run_doc("crisp-attr", CRISP_ATTR);
    run_doc("use-style", USE_STYLE);
    run_doc("empty-svg", EMPTY_SVG);

    // ---- ④ text surface (embedded fixed fontdb fonts) ----
    run_doc("text-basic", TEXT_BASIC);
    run_doc("text-style", TEXT_STYLE);
    probe_text("tp1", TEXT_STYLE);
    run_doc("text-transform", TEXT_TRANSFORM);
    run_doc("text-path", TEXT_PATH);
    run_doc("text-bidi", TEXT_BIDI);
    run_doc("text-deco", TEXT_DECO);
    run_doc("text-fallback", TEXT_FALLBACK);

    // Empty fontdb: no font to resolve -> zero text glyphs (only the background rect renders)
    let optnf = usvg::Options::default();
    let tnf = usvg::Tree::from_str(TEXT_BASIC, &optnf).unwrap();
    let size_nf = tnf.size();
    let mut pmnf = Pixmap::new(W, H).unwrap();
    let ts_nf = Transform::from_scale(W as f32 / size_nf.width(), H as f32 / size_nf.height());
    render_tree(&tnf, ts_nf, &mut pmnf.as_mut());
    // Empty fontdb: no font to resolve -> the whole text element is dropped (found=false), only the base
    let optnf = usvg::Options::default();
    let tnf = usvg::Tree::from_str(TEXT_BASIC, &optnf).unwrap();
    let size_nf = tnf.size();
    let mut pmnf = Pixmap::new(W, H).unwrap();
    let ts_nf = Transform::from_scale(W as f32 / size_nf.width(), H as f32 / size_nf.height());
    render_tree(&tnf, ts_nf, &mut pmnf.as_mut());
    let (found_nf, glyphs_nf) = match tnf.node_by_id("tb1") {
        Some(Node::Text(t)) => (
            true,
            t.layouted().iter().map(|s| s.positioned_glyphs.len()).sum(),
        ),
        _ => (false, 0),
    };
    println!(
        "doc nofonts found={} glyphs={} children={} def fnv={:016x}",
        found_nf,
        glyphs_nf,
        tnf.root().children().len(),
        fnv1a(pmnf.data())
    );

    // ---- ⑤ gzip byte path: a valid .svgz ----
    match usvg::Tree::from_data(GZ_SVG, &opt) {
        Ok(t) => {
            let mut pm = Pixmap::new(W, H).unwrap();
            let size = t.size();
            let ts = Transform::from_scale(W as f32 / size.width(), H as f32 / size.height());
            render_tree(&t, ts, &mut pm.as_mut());
            println!(
                "gzsvg size w={:08x} h={:08x} def fnv={:016x}",
                size.width().to_bits(),
                size.height().to_bits(),
                fnv1a(pm.data())
            );
        }
        Err(e) => println!("gzsvg parse-err {e:?}"),
    }

    // ---- ⑥ six error paths (all deterministic Debug text) ----
    let bad_xml = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10"><rect x="1"/</svg>"##;
    let zero_w = r##"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="10"/>"##;
    let no_attrs = EMPTY_SVG; // no width/height/viewBox: 100% x default_size -> a valid empty tree
    let bad_gzip: &[u8] = &[0x1f, 0x8b, 0x08, 0xde, 0xad, 0xbe, 0xef, 0x00, 0x01];
    let non_utf8: &[u8] = &[0xff, 0xfe, 0x3c, 0x73, 0x76, 0x67];
    let not_svg = "<html><body>not svg</body></html>";
    for (n, bytes) in [
        ("bad-xml", bad_xml.as_bytes()),
        ("zero-width", zero_w.as_bytes()),
        ("no-attrs", no_attrs.as_bytes()),
        ("bad-gzip", bad_gzip),
        ("non-utf8", non_utf8),
        ("not-svg", not_svg.as_bytes()),
    ] {
        match usvg::Tree::from_data(bytes, &opt) {
            Ok(t) => println!(
                "err {n} ok children={} w={:08x}",
                t.root().children().len(),
                t.size().width().to_bits()
            ),
            Err(e) => println!("err {n} {e:?}"),
        }
    }
}
