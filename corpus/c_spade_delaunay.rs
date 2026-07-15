#!/usr/bin/env mirvm
---
[dependencies]
spade = "2"
---
// spade 2：Delaunay 三角化 + robust 精确浮点谓词（orient2d / incircle）差分。
// 固定点集逐个 insert：网格+确定性抖动 / 近共线退化 / 精确共圆(Pythagoras)+1ulp 扰动 /
// 单位圆有理参数点。打印顶点数/三角形数/边数 + 每三角形顶点坐标 to_bits 谱。
// 谱输出：三角形内 3 顶点排序、三角形间排序、顶点集排序——spade 的 DCEL 迭代本是
// Vec 索引序（确定），排序只保证谱纯几何、与内部存储序解耦。
// 再查 locate / locate_vertex / nearest_neighbor / barycentric 权重 + 插入错误路径。
// 坐标生成只用 IEEE 基本运算（加减乘除、u64→f64 舍入）+ splitmix64 整数哈希，
// 不用 sin/cos 等非正确舍入函数，保证 native/mirvm 输入逐比特一致。
use spade::{DelaunayTriangulation, FloatTriangulation, Point2, PositionInTriangulation, Triangulation};

type Tri = DelaunayTriangulation<Point2<f64>>;

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^ (x >> 31)
}

fn bits(p: Point2<f64>) -> (u64, u64) {
    (p.x.to_bits(), p.y.to_bits())
}

fn bits3(pts: [Point2<f64>; 3]) -> [(u64, u64); 3] {
    let mut t = pts.map(bits);
    t.sort();
    t
}

fn pb(p: Point2<f64>) -> String {
    format!("({:016x},{:016x})", p.x.to_bits(), p.y.to_bits())
}

/// ① 7x7 网格 + splitmix64 抖动（∈[-0.09,0.09)）
fn grid_jitter() -> Vec<Point2<f64>> {
    let mut pts = Vec::new();
    for i in 0..7u64 {
        for j in 0..7u64 {
            let h1 = splitmix64(i * 131 + j * 7 + 1);
            let h2 = splitmix64(i * 57 + j * 191 + 2);
            // 取高 53 位 → [0,1) → [-0.09,0.09)
            let jx = ((h1 >> 11) as f64) * (0.18 / 9007199254740992.0) - 0.09;
            let jy = ((h2 >> 11) as f64) * (0.18 / 9007199254740992.0) - 0.09;
            pts.push(Point2::new(i as f64 + jx, j as f64 + jy));
        }
    }
    pts
}

/// ② 近共线：x 轴附近 9 点，y ∈ {-1,0,1}*1e-13 循环，orient2d 小行列式；加两个顶点成三角化
fn near_collinear() -> Vec<Point2<f64>> {
    let mut pts = Vec::new();
    for i in 0..9u64 {
        let y = ((i % 3) as f64 - 1.0) * 1e-13;
        pts.push(Point2::new(i as f64, y));
    }
    pts.push(Point2::new(4.0, 3.0));
    pts.push(Point2::new(4.0, -3.0));
    pts
}

/// ③ 精确共圆：x²+y²=25 上 12 个 Pythagoras 整点（incircle 精确为 0 的退化）
///    + 圆心 + 1ulp 扰动的近共圆点
fn cocircular() -> Vec<Point2<f64>> {
    vec![
        Point2::new(3.0, 4.0),
        Point2::new(4.0, 3.0),
        Point2::new(5.0, 0.0),
        Point2::new(4.0, -3.0),
        Point2::new(3.0, -4.0),
        Point2::new(0.0, -5.0),
        Point2::new(-3.0, -4.0),
        Point2::new(-4.0, -3.0),
        Point2::new(-5.0, 0.0),
        Point2::new(-4.0, 3.0),
        Point2::new(-3.0, 4.0),
        Point2::new(0.0, 5.0),
        Point2::new(0.0, 0.0),
        // 3.0000000000000004 = 3.0 + 1ulp
        Point2::new(f64::from_bits(3.0f64.to_bits() + 1), 4.0),
    ]
}

/// ④ 单位圆有理参数点：t=k/4，x=(1-t²)/(1+t²)，y=2t/(1+t²)（实数域精确共圆）+ 内部 5 点
fn circle_rational() -> Vec<Point2<f64>> {
    let mut pts = Vec::new();
    for k in -3..=3i64 {
        let t = (k as f64) / 4.0;
        let d = 1.0 + t * t;
        pts.push(Point2::new((1.0 - t * t) / d, (2.0 * t) / d));
    }
    for (x, y) in [(0.0, 0.0), (0.5, 0.5), (-0.5, 0.5), (0.5, -0.5), (-0.5, -0.5)] {
        pts.push(Point2::new(x, y));
    }
    pts
}

fn build(name: &str, pts: &[Point2<f64>]) -> Tri {
    let mut tri = Tri::default();
    for (i, &p) in pts.iter().enumerate() {
        match tri.insert(p) {
            Ok(h) => println!("{name} ins[{i}] {} -> v{}", pb(p), h.index()),
            Err(e) => println!("{name} ins[{i}] {} -> ERR {e}", pb(p)),
        }
    }
    tri
}

/// 顶点数/三角形数/边数 + 排序 bits 谱
fn dump_spectrum(name: &str, tri: &Tri) {
    println!(
        "{name} verts={} tris={} undirected_edges={}",
        tri.num_vertices(),
        tri.num_inner_faces(),
        tri.num_undirected_edges()
    );
    let mut vs: Vec<(u64, u64)> = tri.vertices().map(|v| bits(v.position())).collect();
    vs.sort();
    for (x, y) in vs {
        println!("{name} v ({x:016x},{y:016x})");
    }
    let mut tris: Vec<[(u64, u64); 3]> = tri
        .inner_faces()
        .map(|f| bits3(f.vertices().map(|v| v.position())))
        .collect();
    tris.sort();
    for [a, b, c] in tris {
        println!(
            "{name} t ({:016x},{:016x}) ({:016x},{:016x}) ({:016x},{:016x})",
            a.0, a.1, b.0, b.1, c.0, c.1
        );
    }
}

fn dump_locate(name: &str, tri: &Tri, tag: &str, q: Point2<f64>) {
    match tri.locate(q) {
        PositionInTriangulation::OnVertex(h) => {
            println!("loc {name}/{tag} q={} -> OnVertex h={} p={}", pb(q), h.index(), pb(tri.vertex(h).position()));
        }
        PositionInTriangulation::OnEdge(h) => {
            let e = tri.directed_edge(h);
            println!(
                "loc {name}/{tag} q={} -> OnEdge h={} a={} b={}",
                pb(q),
                h.index(),
                pb(e.from().position()),
                pb(e.to().position())
            );
        }
        PositionInTriangulation::OnFace(h) => {
            let f = tri.face(h);
            let ps = bits3(f.vertices().map(|v| v.position()));
            println!(
                "loc {name}/{tag} q={} -> OnFace h={} tri=({:016x},{:016x}) ({:016x},{:016x}) ({:016x},{:016x})",
                pb(q),
                h.index(),
                ps[0].0, ps[0].1, ps[1].0, ps[1].1, ps[2].0, ps[2].1
            );
        }
        PositionInTriangulation::OutsideOfConvexHull(h) => {
            let e = tri.directed_edge(h);
            println!(
                "loc {name}/{tag} q={} -> OutsideOfConvexHull h={} a={} b={}",
                pb(q),
                h.index(),
                pb(e.from().position()),
                pb(e.to().position())
            );
        }
        PositionInTriangulation::NoTriangulation => {
            println!("loc {name}/{tag} q={} -> NoTriangulation", pb(q));
        }
    }
}

fn main() {
    // ===== 空/单点退化 =====
    let mut empty = Tri::default();
    dump_locate("empty", &empty, "e0", Point2::new(1.0, 2.0));
    empty.insert(Point2::new(1.0, 2.0)).unwrap();
    dump_locate("empty", &empty, "e1-on", Point2::new(1.0, 2.0));
    dump_locate("empty", &empty, "e2-off", Point2::new(9.0, 9.0));
    println!("empty nn={:?}", empty.nearest_neighbor(Point2::new(0.0, 0.0)).map(|v| v.fix().index()));
    let really_empty = Tri::default();
    println!("really-empty nn={:?}", really_empty.nearest_neighbor(Point2::new(0.0, 0.0)).map(|v| v.fix().index()));

    // ===== 四个点集：构建 + 谱 =====
    let sets: Vec<(&str, Vec<Point2<f64>>)> = vec![
        ("grid", grid_jitter()),
        ("collinear", near_collinear()),
        ("cocirc", cocircular()),
        ("circle", circle_rational()),
    ];
    let mut tris = Vec::new();
    for (name, pts) in &sets {
        let tri = build(name, pts);
        dump_spectrum(name, &tri);
        tris.push((name, tri));
    }

    // ===== locate 定点查询 =====
    // grid：命中顶点（首插入点）/ 面内 / 凸包外
    let grid = &tris[0].1;
    let g0 = grid_jitter()[0];
    dump_locate("grid", grid, "vertex", g0);
    dump_locate("grid", grid, "face", Point2::new(2.5, 2.5));
    dump_locate("grid", grid, "outside", Point2::new(100.0, -50.0));
    // 边中点：取第一个 inner face 的首条边，f64 中点必落在该边近旁（谓词精确判定 OnEdge/OnFace）
    let e0 = grid.inner_faces().next().unwrap().adjacent_edge();
    let (ea, eb) = (e0.from().position(), e0.to().position());
    let mid = Point2::new((ea.x + eb.x) / 2.0, (ea.y + eb.y) / 2.0);
    dump_locate("grid", grid, "edgemid", mid);
    // collinear：轴上点 / 轴外点
    let coll = &tris[1].1;
    dump_locate("collinear", coll, "onaxis", Point2::new(4.0, 0.0));
    dump_locate("collinear", coll, "nearaxis", Point2::new(4.0, 1e-14));
    dump_locate("collinear", coll, "outside", Point2::new(-3.0, 5.0));
    // cocirc：圆心（命中顶点）/ 共圆点 / 近圆心面内 / 凸包外
    let cc = &tris[2].1;
    dump_locate("cocirc", cc, "center", Point2::new(0.0, 0.0));
    dump_locate("cocirc", cc, "oncirc", Point2::new(-4.0, 3.0));
    dump_locate("cocirc", cc, "inner", Point2::new(0.5, 0.5));
    dump_locate("cocirc", cc, "outside", Point2::new(6.0, 0.0));
    // circle：有理圆点 / 内部点
    let ci = &tris[3].1;
    dump_locate("circle", ci, "oncirc", Point2::new(1.0, 0.0));
    dump_locate("circle", ci, "inner", Point2::new(0.1, -0.2));
    dump_locate("circle", ci, "outside", Point2::new(0.0, 2.0));

    // locate_vertex：存在 / 不存在
    println!("lv grid hit={:?}", grid.locate_vertex(g0).map(|v| v.fix().index()));
    println!("lv grid miss={:?}", grid.locate_vertex(Point2::new(0.12345, 6.54321)).map(|v| v.fix().index()));

    // nearest_neighbor：两个定点 → 顶点句柄+坐标 bits
    for (i, q) in [Point2::new(1.4, 2.6), Point2::new(-10.0, 10.0)].iter().enumerate() {
        let v = grid.nearest_neighbor(*q).unwrap();
        println!("nn grid[{i}] q={} -> h={} p={}", pb(*q), v.fix().index(), pb(v.position()));
    }

    // barycentric 权重：面内点 / 顶点上 / 凸包外（权重数 3/1/0），按句柄排序
    let bary = grid.barycentric();
    for (i, q) in [Point2::new(2.5, 2.5), g0, Point2::new(100.0, -50.0)].iter().enumerate() {
        let mut ws = Vec::new();
        bary.get_weights(*q, &mut ws);
        ws.sort_by_key(|(h, _)| h.index());
        let body: Vec<String> = ws.iter().map(|(h, w)| format!("v{}:{:016x}", h.index(), w.to_bits())).collect();
        println!("bary grid[{i}] q={} n={} [{}]", pb(*q), ws.len(), body.join(" "));
    }

    // ===== 插入错误路径 + 重复点 =====
    let mut errs = Tri::default();
    errs.insert(Point2::new(0.0, 0.0)).unwrap();
    println!("err nan = {}", errs.insert(Point2::new(f64::NAN, 0.0)).unwrap_err());
    println!("err toolarge = {}", errs.insert(Point2::new(1e300, 0.0)).unwrap_err());
    println!("err toosmall = {}", errs.insert(Point2::new(0.0, 1e-300)).unwrap_err());
    println!("err neg-toolarge = {}", errs.insert(Point2::new(-1e61, 0.0)).unwrap_err());
    // 重复插入：返回已存在顶点，顶点数不变
    let mut dup = Tri::default();
    let h0 = dup.insert(Point2::new(1.5, 2.5)).unwrap();
    let h1 = dup.insert(Point2::new(1.5, 2.5)).unwrap();
    println!("dup same={} verts={}", h0 == h1, dup.num_vertices());
}
