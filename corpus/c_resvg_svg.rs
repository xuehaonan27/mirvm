#!/usr/bin/env mirvm
---
[dependencies]
# 钉 0.44.0 配对 0.11.4。绕行记录（语义不变）：resvg 0.44 的 API 与本文件
# 用法兼容（render(tree, transform, &mut PixmapMut)，anti_alias 由
# ShapeRendering 决定），但它依赖 `tiny-skia = "0.11.4"` 且带默认特性
# （simd 开），其 f32x4/f32x8 光栅管线在本 nightly core_arch 下走外链 LLVM
# 内部符号（`_mm_max_ps` → `llvm.x86.sse.max.ps`、cvt/round/rcp 同族），
# mirvm 未内建 → `TRAP: foreign llvm.x86.sse.max.ps` 进程退出（探针复核）。
# cargo features 向下不可减——resvg 对 tiny-skia 的 dep 边带默认特性，下游
# 关不掉。按任务书退路改 usvg+tiny-skia 直连：tiny-skia scalar
# （default-features=false，c_tiny_skia 路线三维已绿）。渲染语义由下方
# mini 渲染器对齐 resvg 0.44 的 src/{render,path,clip,geom}.rs（shapes/
# 渐变/groups/opacity/clip/blend 子集逐行移植；filters/masks/images/
# patterns 不实现，本文档集不含）。
usvg = { version = "=0.44.0", default-features = false }
# usvg 全关 text/system-fonts/memmap-fonts → 无字体/塑形栈；其必需依赖
# flate2（miniz_oxide 纯 Rust 后端）顺带覆盖 .svgz gzip 解压路径。
tiny-skia = { version = "=0.11.4", default-features = false, features = ["std"] }
---
// resvg/usvg 0.44 shapes-only SVG 渲染差分（无 text 节点 → 无需字体）。
// 九类内嵌固定文档：prim（linear+radial 渐变 userSpaceOnUse/三停点/stop-opacity/
// reflect spread、rounded rect、evenodd 自交贝塞尔、dash 描边奇数段、transform
// 组、opacity 组、clipPath 组、mix-blend-mode 组、visibility=hidden、fill=none）、
// vb50（viewBox 0.5 倍缩放、polygon、objectBoundingBox 渐变）、par 系四文档
// （竖幅 viewBox × xMidYMid meet/slice/none/xMinYMax）、novh（无 width/height
// 仅 viewBox：usvg 100% → viewBox 尺寸解析分支）、crisp-attr（元素级
// shape-rendering=crispEdges/optimizeSpeed）、use-style（use x/y/opacity、
// style 属性展示样式、polyline）、gzsvg（gzip 字节 from_data 解压渲染）。
// 每文档三处配置渲染 160x120：默认（GeometricPrecision=AA 开）/ 无 AA
// （Options::shape_rendering=CrispEdges）/ to_string 重解析 roundtrip
// （渲染 FNV 须与默认相等）。打印整像素 FNV-1a + 固定坐标抽样像素 RGBA。
// 树 API 探针：size 位型、children 数、node_by_id 的 abs_transform/bounding
// box 位级、缺失 id。错误路径六条：坏 XML / width=0（InvalidSize）/ 空属性
// <svg/>（100%x100% 默认尺寸、合法空渲染）/ 坏 gzip / 非 UTF-8 / 非 svg 根。
// 确定性：全固定常量；浮点一律 to_bits；无随机/时间/地址/HashMap 迭代；
// 成功路径 stderr 为空。
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

// ===== mini 渲染器：resvg 0.44 shapes/渐变/groups/clip 子集移植 =====

/// resvg geom::fit_to_rect 原样。
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

/// resvg::render 的 max_bbox（按顶层画布推导的常量）。
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

fn render_node(node: &Node, ts: Transform, pm: &mut PixmapMut) {
    match node {
        Node::Group(group) => render_group(group, ts, pm),
        Node::Path(path) => render_path(path, ts, pm),
        Node::Image(_) | Node::Text(_) => {} // 本文档集不含 raster/text
    }
}

/// resvg 0.44 render::render_group 的 shapes 子集（filters/mask 恒空分支）。
fn render_group(group: &usvg::Group, ts: Transform, pm: &mut PixmapMut) {
    let ts = ts.pre_concat(group.transform());
    if !group.should_isolate() {
        render_nodes(group, ts, pm);
        return;
    }
    let Some(bbox) = group.layer_bounding_box().transform(ts) else {
        return;
    };
    // filters 恒空 → 简单外扩 2px 分支 + fit_to_rect（同 resvg）
    let Some(ibbox) = IntRect::from_xywh(
        bbox.x().floor() as i32 - 2,
        bbox.y().floor() as i32 - 2,
        bbox.width().ceil() as u32 + 4,
        bbox.height().ceil() as u32 + 4,
    )
    .and_then(|r| fit_to_rect(r, max_bbox()))
    else {
        return;
    };
    let shift_ts = {
        let mut dx = bbox.x();
        let mut dy = bbox.y();
        dx -= bbox.x() - ibbox.x() as f32;
        dy -= bbox.y() - ibbox.y() as f32;
        Transform::from_translate(-dx, -dy)
    };
    let ts = shift_ts.pre_concat(ts);
    let Some(mut sub) = Pixmap::new(ibbox.width(), ibbox.height()) else {
        return;
    };
    render_nodes(group, ts, &mut sub.as_mut());
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
}

/// resvg 0.44 render::convert_blend_mode 原样（16 arm 全映射）。
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

/// resvg 0.44 path::render 的 paint_order 分派。
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

/// resvg 0.44 path::fill_path 的 shapes 子集（pattern arm 简化为跳过）。
fn fill_path(path: &usvg::Path, mode: BlendMode, ts: Transform, pm: &mut PixmapMut) -> Option<()> {
    let fill = path.fill()?;
    // 水平/垂直线不可填充（同 resvg 提前返回）
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
        usvg::Paint::Pattern(_) => return None, // 本文档集不含 pattern
    }
    paint.anti_alias = path.rendering_mode().use_shape_antialiasing();
    paint.blend_mode = mode;
    pm.fill_path(path.data(), &paint, rule, ts, None);
    Some(())
}

/// resvg 0.44 path::stroke_path 的 shapes 子集。
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

/// resvg 0.44 的 convert_linear_gradient / convert_radial_gradient。
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
        (rg.cx(), rg.cy()).into(),
        rg.r().get(),
        stops,
        mode,
        rg.transform(),
    )
}

/// resvg 0.44 convert_base_gradient：stops 透明度 = stop.opacity × fill/stroke opacity。
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

/// resvg 0.44 clip::apply 原样（Clear 画子形状 → 反相 → apply_mask）。
fn clip_apply(clip: &usvg::ClipPath, ts: Transform, pm: &mut Pixmap) {
    let mut clip_pm = Pixmap::new(pm.width(), pm.height()).unwrap();
    clip_pm.fill(Color::BLACK);
    clip_draw_children(clip.root(), ts.pre_concat(clip.transform()), &mut clip_pm.as_mut());
    if let Some(nested) = clip.clip_path() {
        clip_apply(nested, ts, pm);
    }
    let mut mask = Mask::from_pixmap(clip_pm.as_ref(), MaskType::Alpha);
    mask.invert();
    pm.apply_mask(&mask);
}

/// resvg 0.44 clip 的 draw_children（Path / 无 clip 的 Group 两臂）。
fn clip_draw_children(parent: &usvg::Group, ts: Transform, pm: &mut PixmapMut) {
    for child in parent.children() {
        match child {
            Node::Path(path) => {
                if path.is_visible() {
                    fill_path(path, BlendMode::Clear, ts, pm);
                }
            }
            Node::Group(group) => {
                clip_draw_children(group, ts.pre_concat(group.transform()), pm);
            }
            Node::Image(_) | Node::Text(_) => {}
        }
    }
}

// ===== 文档集（shapes-only，无 text/image/filter/mask/pattern）=====

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

/// 竖幅内容（viewBox 120x240）× preserveAspectRatio 谱系。
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

/// gzip(deflate, mtime=0) 预压缩的小 SVG：from_data 的 gunzip 路径。
const GZ_SVG: &[u8] = &[
    31, 139, 8, 0, 0, 0, 0, 0, 2, 3, 77, 141, 193, 14, 194, 32, 16, 68, 127, 101, 179, 158, 133,
    181, 88, 15, 6, 248, 23, 163, 20, 136, 104, 13, 108, 220, 250, 247, 162, 73, 141, 151, 201,
    204, 228, 77, 198, 182, 103, 132, 229, 86, 238, 205, 97, 98, 126, 28, 181, 22, 17, 37, 70,
    205, 53, 234, 129, 136, 116, 39, 16, 36, 95, 56, 57, 220, 29, 16, 82, 200, 49, 113, 247, 3,
    122, 91, 195, 153, 97, 233, 1, 225, 245, 213, 21, 220, 255, 129, 132, 48, 229, 82, 28, 110,
    204, 116, 162, 177, 199, 198, 117, 190, 134, 94, 4, 10, 100, 126, 197, 118, 93, 171, 17, 181,
    183, 159, 107, 255, 6, 251, 62, 160, 209, 162, 0, 0, 0,
];

/// 全局抽样坐标（每文档同一组）。
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

/// 解析 → 两配置渲染 → to_string roundtrip 渲染对比 → 抽样像素。
fn run_doc(name: &str, svg: &str) {
    let opt = usvg::Options::default();
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

    // 无 AA 配置：Options::shape_rendering = CrispEdges（resvg 同款 AA 决策来源）
    let mut opt2 = usvg::Options::default();
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

    // to_string → 重解析 → 重渲染，语义 FNV 须与默认一致
    let s = tree.to_string(&usvg::WriteOptions::default());
    let eq = match usvg::Tree::from_str(&s, &opt) {
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

fn main() {
    run_doc("prim", PRIM);

    // prim 的树 API 位级探针：id 查找 / abs_transform / abs_bounding_box
    let opt = usvg::Options::default();
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

    // gzip 字节路径：合法 .svgz
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

    // 错误路径六条（全部确定性 Debug 文本）
    let bad_xml = r##"<svg xmlns="http://www.w3.org/2000/svg" width="10"><rect x="1"/</svg>"##;
    let zero_w = r##"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="10"/>"##;
    let no_attrs = EMPTY_SVG; // 无 width/height/viewBox：100% × default_size → 合法空树
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
