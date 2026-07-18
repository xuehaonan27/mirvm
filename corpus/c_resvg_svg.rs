#!/usr/bin/env mirvm
---
[dependencies]
# 钉 =0.47.0（2026-07-18 cargo search 实勘 crates.io max stable；resvg/usvg/
# tiny-skia 同仓 linebender/resvg 0.47 release train）。default-features=false
# + features=["text"]：text 拉 fontdb 0.23.0 / rustybuzz 0.20.1 / ttf-parser
# 0.25.1(gvar-alloc) / unicode-bidi / unicode-script / unicode-vo；
# system-fonts 与 memmap-fonts 保持关 → 不扫系统字体目录、不 mmap 字体文件。
usvg = { version = "=0.47.0", default-features = false, features = ["text"] }
# tiny-skia 标量后端（与 usvg 的 tiny-skia-path 0.12.0 依赖边同源配对）。
# 绕行记录（语义不变，批6 同型退路的 0.47 复核）：resvg 0.47 依赖
# `tiny-skia = "0.12.0"` 且 dep 边带默认特性（simd 开），其 f32x4/f32x8
# 光栅管线在本 nightly core_arch 下走外链 LLVM 内部符号（`_mm_max_ps` →
# `llvm.x86.sse.max.ps` 同族），mirvm 未内建 → TRAP（批6 头注探针实证 0.44
# 同构；resvg-0.47.0/Cargo.toml 的 tiny-skia dep 边复核仍无
# default-features=false）。cargo features 向下不可减——resvg 对 tiny-skia
# 的 dep 边下游关不掉。按批6 同款退路改 usvg+tiny-skia 直连：tiny-skia
# scalar（c_tiny_skia 路线三维已绿）。渲染语义由下方 mini 渲染器对齐
# resvg 0.47 的 src/{render,path,clip,geom}.rs 逐行移植（含 render.rs 的
# Node::Text(text) => render_group(text.flattened()) 分派——usvg 解析期已
# 完成塑形/布局/字形轮廓化，flattened 即普通 group/path 树）；
# filters/masks/images/patterns 不实现，本文档集不含。
tiny-skia = { version = "=0.12.0", default-features = false, features = ["std"] }
# fontdb 定值字源：17 个 include_bytes! 内嵌字体（Libertinus Serif×6 /
# NewCM Math×3 / NewCM10×4 / DejaVu Sans Mono×4），不触系统字体——
# c_typst_pdf（批10 波1）同款字体源先例。
typst-assets = { version = "=0.15.1", features = ["fonts"] }
---
// c_resvg_svg —— resvg/usvg 0.47 完整 SVG 渲染三维差分（批10 波2，规格
// docs/corpus.md §7）：路径/渐变/文本（内嵌 fontdb 定值字体）光栅像素
// FNV，tiny-skia/fontdue 已通的上层接棒。批6 shapes-only 同名 driver 升级：
// 依赖线 0.44→0.47，新增文本塑形/布局/装饰/textPath/bidi/回退面。
//
// 【状态：expected-red（C 维引擎红）】A（mirvm 默认）/ B（native）两维全绿
// 且 stdout 141 行逐字节一致、stderr 真空（0 字节）、exit 全 0——driver
// 本体确定性经 A==B 逐字节验证无虞；C（MIRVM_JIT_THRESHOLD=1）在
// text-style 文档 def 渲染处 panic（exit 101），诊断链如下。
//
// 红因（JIT 误编译，供养侧判定）：JIT 生成代码在 hairline 描边渲染路径上
// 产出与解释器/native 分叉的值，使 tiny-skia 定点斜率越界触发断言：
//   tiny-skia-0.12.0/src/scan/hairline_aa.rs:473:13
//   assertion failed: slope <= fdot16::ONE && slope >= -fdot16::ONE
// （381 行 "mostly horizontal" 分支为对称同族断言，算子序相反；最小复现
// 热跑站点漂移至 381，全量 driver 冷热两跑均稳定 473）。
// 判定证据链：
//   1) 解释器输出与 native 逐位一致——含同一 hairline 描边文本的像素
//      FNV 相同（A==B 141 行逐字节），故输入数据与算法无分叉；
//   2) fast_div 为纯整数运算（left_shift(a,16)/b），断言越界的唯一可能
//      是 f32→fdot6 定点转换链的输入值在 JIT 下分叉（|slope|≤1 由分支
//      条件 |dx|≥|dy|/|dy|>|dx| 数学保证，native dev profile 断言开与
//      解释器均不触发）；
//   3) 踩雷与否随被 JIT 编译的函数集合/序漂移：最小化中 t6（首行三
//      tspan）中而 t7（t6+一行无关文本）不中；同字形同坐标的单 glyph
//      'o' 渲染不中——提示编译序敏感的末位 ulp/聚合值差异在定点转换
//      边界被放大为 ±1 fdot6。
//   触发链：text-style 首行 Libertinus Serif Bold 24px、stroke-width=0.6
//   （hairline stroker → hairline AA 填充）、串 "GradItaBo" 第 9 glyph
//   'o'（x≈108.688）处。最小复现 /tmp/repro_resvg_svg.rs（8 轮 23 变体
//   二分；其 native/解释器 fnv=417be013020ecd27 一致，JIT=1 exit=101）。
//   red_code=101（Rust panic 退出码）
//   red_pattern=「panicked at .*tiny-skia-0.12.0/src/scan/hairline_aa.rs:」
//   +「assertion failed: slope」（站点号 381/473 随 JIT 缓存态漂移，同族）
// C 维现场：stdout 止于 "doc text-style size ..." 行（def 渲染 panic，
// 95 行），两跑 stdout 逐字节稳定，stderr 4 行 panic 文，exit=101。
//
// 版本钉（相容组合证据）：见 frontmatter 行内注。三直挂依赖全 = 钉死；
// usvg 0.47.0 自身锁定 fontdb "0.23.0"/rustybuzz "0.20.1"/ttf-parser
// "0.25.1"/tiny-skia-path "0.12.0"/kurbo "0.13.0" 等传递约束，与直挂
// tiny-skia 0.12.0（path 0.12.0）同源无跨线错配；rustybuzz 的 wasmi
// （wasm-shaper）为可选依赖且默认关。
//
// 确定性说明：
//   * 字体字节：typst_assets::fonts() 17 个定值资产，逐文件 len+FNV-1a
//     锚定；装载序=数组序（fontdb faces 为 Vec 插入序，face 转储逐行锚）。
//   * 塑形/布局：rustybuzz 0.20.1（default features=["std"]）对固定字体
//     字节+固定字符串为纯函数；unicode-bidi 重排、ttf-parser/kurbo 轮廓
//     均为位确定计算；无 OS 随机/壁钟/时区/网络。
//   * 回退链：text-fallback 文档求不存在的族 → usvg 固定回退（Options
//     默认 font_family="Times New Roman" 不在库 → fontdb 通用回退），库
//     定值故结果定值；nofonts 文档空 fontdb → 文本元素整颗丢弃
//     （found=false/children 锚定）。
//   * 两处原生实测即确定的事实锚（三维跑同一代码，值本体即锚，不要求
//     语义"正确"）：① text-deco 的 roundtrip eq=false——usvg 0.47 writer
//     不保 text-decoration（装饰信息在 to_string 丢失，重解析渲染不同）；
//     ② text-basic/style/transform/bidi/deco/fallback 六文档 def fnv ==
//     crisp fnv——字形 AA 由 text-rendering 决定（Options.shape_rendering
//     不影响 glyph 路径），且底色 rect 全像素对齐（AA 不变量）；text-path
//     因含形状描边 def!=crisp，与形状面九文档同证 crisp 配置真实生效。
//   * 渲染：tiny-skia 标量后端逐位 IEEE；每文档整图 FNV-1a + 8 固定坐标
//     抽样像素 RGBA hex；浮点打印一律 to_bits；探针全走 Vec/切片，无
//     HashMap 迭代序出口。
//   * usvg/tiny-skia/rustybuzz 经 log crate 打警告，无 subscriber →
//     stderr 真空。
//
// 覆盖清单：
//   形状面（批6 继承，0.47 移植）：prim（linear+radial 渐变
//   userSpaceOnUse/三停点/stop-opacity/reflect spread、rounded rect、
//   evenodd 自交贝塞尔、dash 描边奇数段、transform 组、opacity 组、
//   clipPath 组、mix-blend-mode 组、visibility=hidden、fill=none）、
//   vb50（viewBox 0.5 倍缩放、polygon、objectBoundingBox 渐变）、par×4
//   （竖幅 viewBox × xMidYMid meet/slice/none/xMinYMax）、novh（无
//   width/height 仅 viewBox）、crisp-attr（元素级 shape-rendering=
//   crispEdges/optimizeSpeed）、use-style（use x/y/opacity、style 展示
//   属性、polyline）、empty-svg（100% 默认尺寸）、gzsvg（gzip from_data）。
//   文本面（波2 新增，全内嵌字体）：text-basic（start/middle/end 三
//   anchor、kerning 串 "AV To Kern"、text-rendering=optimizeSpeed、
//   衬线/等宽两族）、text-style（tspan 渐变 fill/italic/bold+stroke、
//   letter/word-spacing、textLength spacingAndGlyphs）、text-transform
//   （dx/dy/rotate 逐字数组、组 rotate、small-caps、baseline-shift
//   super/sub）、text-path（textPath 曲线排布 startOffset）、text-bidi
//   （direction=rtl 希伯来混排重排、writing-mode=tb 竖排含缺字 CJK
//   探测）、text-deco（underline/overline/line-through 装饰路径）、
//   text-fallback（缺字族回退链）、nofonts（空 fontdb 文本元素丢弃锚）。
//   探针：字体文件字节锚 ×17、fontdb faces 全量转储、树 API 位级（p1
//   path 与 tp1 text 的 bbox/abs_transform bits、chunks/spans/layouted
//   字形 id+文本+font id、flattened 子节点数、tree.fontdb 面数）、错误
//   路径六条（坏 XML / width=0 / 空属性 / 坏 gzip / 非 UTF-8 / 非 svg 根）。
//   每文档三配置渲染 160x120：默认（AA 开）/ 无 AA（Options::
//   shape_rendering=CrispEdges）/ to_string 重解析 roundtrip（渲染 FNV
//   与默认比对，eq 值本体即锚）。
//
// 复红定因参照（三维复跑）：
//   A: target/release/mirvm run corpus/c_resvg_svg.rs
//   B: d=$(grep -l 'name = "c_resvg_svg"' ~/.cache/mirvm/scripts/*/Cargo.toml | xargs dirname) && cd "$d" && cargo +nightly-2026-07-02 run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run corpus/c_resvg_svg.rs
// 三维实测（2026-07-18，同机）：
//   A（mirvm 默认）：exit 0，stdout 141 行，stderr 0 字节，real ~36s
//     （deps 共享缓存热跑）。
//   B（cargo +nightly-2026-07-02 run -q，script dir 内）：exit 0，
//     stdout 141 行，stderr 0 字节，real ~8.5s。
//   A==B 逐字节一致。锚点摘抄：faces len=17；prim def fnv=
//     7412fbca454bcd60；text-basic def fnv=ce49adf0e33fcbc0；probe tp1
//     layouted spans=3 glyphs=11（g0 id=40 'G' font=LibertinusSerif-
//     Regular）；text-deco rt eq=false（usvg writer 丢 text-decoration
//     的确定性事实锚）；nofonts found=false children=1。
//   C（MIRVM_JIT_THRESHOLD=1）：exit 101（JIT 误编译 panic，见上红因），
//     stdout 95 行止于 text-style def，两跑逐字节稳定，real ~13s。
//   依赖闭包 49 crate（usvg 0.47.0 / rustybuzz 0.20.1 / fontdb 0.23.0 /
//   ttf-parser 0.25.1 / unicode-* / kurbo 0.13.0 / tiny-skia 0.12.0 标量 /
//   typst-assets 0.15.1 等；script dir Cargo.lock 实数）。
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

/// 定值 Options：空 fontdb + typst-assets 内嵌 17 字体（装载序=数组序）。
/// 每次解析新建（Options 不可 Clone），同一固定输入 → 同一 DB 状态。
fn make_opt() -> usvg::Options<'static> {
    let mut opt = usvg::Options::default();
    for f in typst_assets::fonts() {
        opt.fontdb_mut().load_font_data(f.to_vec());
    }
    opt
}

// ===== mini 渲染器：resvg 0.47 shapes/text 子集移植 =====

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

/// resvg 0.47 render::render_node（Image 臂跳过：本文档集不含 raster）。
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

/// resvg 0.47 render::render_group 的 filters/mask 恒空分支。
fn render_group(group: &usvg::Group, ts: Transform, pm: &mut PixmapMut) -> Option<()> {
    let ts = ts.pre_concat(group.transform());
    if !group.should_isolate() {
        render_nodes(group, ts, pm);
        return Some(());
    }
    let bbox = group.layer_bounding_box().transform(ts)?;
    // filters 恒空 → 外扩 2px 分支 + fit_to_rect（同 resvg）
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
    // filters 恒空 → 无 filter 应用；mask 恒 None → 跳过
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

/// resvg 0.47 render::convert_blend_mode 原样（16 arm 全映射）。
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

/// resvg 0.47 path::render 的 paint_order 分派。
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

/// resvg 0.47 path::fill_path 的 shapes 子集（pattern arm 简化为跳过）。
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

/// resvg 0.47 path::stroke_path 的 shapes 子集。
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

/// resvg 0.47 的 convert_linear_gradient / convert_radial_gradient。
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

/// resvg 0.47 convert_base_gradient：stops 透明度 = stop.opacity × fill/stroke opacity。
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

/// resvg 0.47 clip::apply 原样（Clear 画子形状 → 反相 → apply_mask）。
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

/// resvg 0.47 clip::draw_children 原样（Path / Text flattened / Group 三臂）。
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

/// resvg 0.47 clip::clip_group 原样（SourceOver 画组 → 裁剪 → Xor 合成）。
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

// ===== 文档集 A：shapes（批6 继承）=====

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

// ===== 文档集 B：text（波2 新增，内嵌字体族：Libertinus Serif / DejaVu Sans Mono）=====

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

    // 无 AA 配置：Options::shape_rendering = CrispEdges（resvg 同款 AA 决策来源）
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

    // to_string → 重解析 → 重渲染，渲染 FNV 与默认比对（eq 值本体即锚）
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

/// text 节点的树 API 位级探针：chunks/spans/layouted 字形/flattened/bbox bits。
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
    // ---- ① 字体字节锚：内嵌资产（装载序 = 数组序）----
    let mut total = 0usize;
    let mut count = 0usize;
    for (i, f) in typst_assets::fonts().enumerate() {
        println!("font[{i:02}] len={} fnv={:016x}", f.len(), fnv1a(f));
        total += f.len();
        count += 1;
    }
    println!("font total files={count} bytes={total}");

    // ---- ② fontdb 面：faces 全量转储（Vec 插入序）----
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

    // ---- ③ 形状面（批6 继承）----
    run_doc("prim", PRIM);

    // prim 的树 API 位级探针：id 查找 / abs_transform / abs_bounding_box
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

    // ---- ④ 文本面（内嵌 fontdb 定值字体）----
    run_doc("text-basic", TEXT_BASIC);
    run_doc("text-style", TEXT_STYLE);
    probe_text("tp1", TEXT_STYLE);
    run_doc("text-transform", TEXT_TRANSFORM);
    run_doc("text-path", TEXT_PATH);
    run_doc("text-bidi", TEXT_BIDI);
    run_doc("text-deco", TEXT_DECO);
    run_doc("text-fallback", TEXT_FALLBACK);

    // 空 fontdb：无字体可解析 → 文本零字形（仅底色矩形渲染）
    let optnf = usvg::Options::default();
    let tnf = usvg::Tree::from_str(TEXT_BASIC, &optnf).unwrap();
    let size_nf = tnf.size();
    let mut pmnf = Pixmap::new(W, H).unwrap();
    let ts_nf = Transform::from_scale(W as f32 / size_nf.width(), H as f32 / size_nf.height());
    render_tree(&tnf, ts_nf, &mut pmnf.as_mut());
    // 空 fontdb：无字体可解析 → 文本元素整颗丢弃（found=false 锚），仅底色渲染
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

    // ---- ⑤ gzip 字节路径：合法 .svgz ----
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

    // ---- ⑥ 错误路径六条（全部确定性 Debug 文本）----
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
