#!/usr/bin/env mirvm
---
[dependencies]
# default-features=false：默认 features 里的 multithreading 会把 rayon 拉进
# i_overlay 的 BooleanOps 并行归并——差分要最小调度面（协作调度器上的浮点
# 归并无额外信号），本 driver 全部目标 API 均在核心路径（area/centroid/
# contains/coordinate_position/line_intersection/convex_hull/simplify/
# BooleanOps/haversine/vincenty）。
geo = { version = "0.29", default-features = false }
---
// geo 0.29：地理几何浮点重差分。覆盖：
// ① 多边形 unsigned/signed area、centroid（含空多边形 None 路径）、
//    perimeter——geo 0.29 无 Perimeter trait，以 exterior+interiors 的
//    Euclidean length 和手算；含带孔多边形。
// ② point-in-polygon：contains 正反例 + CoordinatePosition 三态细分
//    （内部/外部/孔内/边上/顶点）。
// ③ line_intersection 四形态：proper SinglePoint / 端点非 proper /
//    平行 None / 共线 Collinear。
// ④ 定种 xorshift 点集 convex hull（quick_hull）+ 对角共线点。
// ⑤ Douglas-Peucker simplify 固定 epsilon=0.1（抖动折线 + 尖刺）。
// ⑥ BooleanOps：union/intersection/difference/xor 两个固定多边形，
//    打印子多边形数 + 顶点总计数 + 结果面积 bits。
// ⑦ Haversine（新 line_measures API：Haversine::distance——旧
//    HaversineDistance trait 0.29 已 deprecated，零 warning 纪律绕行）
//    与 Vincenty 距离两城市坐标对 + 近对跖点不收敛错误路径。
// geo 0.29 无 buffer/offset API（全 crate 无 Buffer trait），跳过。
// 确定性：浮点一律 to_bits hex 打印；点集定种；无 HashMap 迭代/时间/地址。
use geo::coordinate_position::CoordPos;
use geo::line_intersection::{line_intersection, LineIntersection};
use geo::{
    point, polygon, Area, BooleanOps, Centroid, Contains, ConvexHull, CoordinatePosition,
    Distance, Euclidean, Haversine, Length, Line, LineString, MultiPoint, Point, Polygon,
    Simplify, VincentyDistance,
};

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

    /// [0,100) 内 1/16 网格伪随机坐标（整数取模，位级确定）。
    fn coord(&mut self) -> f64 {
        (self.next() % 1600) as f64 / 16.0
    }
}

fn pb(v: f64) -> String {
    format!("{:016x}", v.to_bits())
}

fn pxy(p: &Point<f64>) -> String {
    format!("{}:{}", pb(p.x()), pb(p.y()))
}

/// perimeter = 外环 + 各内环的 Euclidean length 和（geo 0.29 无 Perimeter trait）。
fn perimeter(poly: &Polygon<f64>) -> f64 {
    let mut p = poly.exterior().length::<Euclidean>();
    for r in poly.interiors() {
        p += r.length::<Euclidean>();
    }
    p
}

fn dump_poly(label: &str, poly: &Polygon<f64>) {
    println!(
        "{label} uarea={} sarea={} peri={}",
        pb(poly.unsigned_area()),
        pb(poly.signed_area()),
        pb(perimeter(poly))
    );
    match poly.centroid() {
        Some(p) => println!("{label} centroid={}", pxy(&p)),
        None => println!("{label} centroid=None"),
    }
}

fn main() {
    // ① area/centroid/perimeter：简单五边形、带孔矩形、空多边形（None 路径）
    let simple = polygon![
        (x: 0.0, y: 0.0), (x: 4.0, y: 0.5), (x: 3.5, y: 3.0),
        (x: 1.5, y: 4.0), (x: -0.5, y: 2.0),
    ];
    let holed = polygon![
        exterior: [
            (x: 0.0, y: 0.0), (x: 10.0, y: 0.0), (x: 10.0, y: 10.0),
            (x: 0.0, y: 10.0), (x: 0.0, y: 0.0),
        ],
        interiors: [[
            (x: 3.0, y: 3.0), (x: 7.0, y: 3.0), (x: 7.0, y: 7.0),
            (x: 3.0, y: 7.0), (x: 3.0, y: 3.0),
        ]],
    ];
    let empty: Polygon<f64> = Polygon::new(LineString::new(Vec::new()), Vec::new());
    dump_poly("simple", &simple);
    dump_poly("holed", &holed);
    dump_poly("empty", &empty);

    // ② point-in-polygon：contains 正反例 + 三态位置细分
    for (label, q) in [
        ("inside", point!(x: 5.0, y: 2.0)),
        ("outside", point!(x: 20.0, y: 20.0)),
        ("in-hole", point!(x: 5.0, y: 5.0)),
        ("boundary", point!(x: 0.0, y: 5.0)),
        ("vertex", point!(x: 10.0, y: 10.0)),
    ] {
        let pos = match holed.coordinate_position(&q.0) {
            CoordPos::Inside => "Inside",
            CoordPos::OnBoundary => "OnBoundary",
            CoordPos::Outside => "Outside",
        };
        println!("pip {label} contains={} pos={pos}", holed.contains(&q));
    }

    // ③ line_intersection 四形态
    let cases = [
        (
            "proper",
            Line::new(point!(x: 0.0, y: 0.0), point!(x: 5.0, y: 5.0)),
            Line::new(point!(x: 0.0, y: 5.0), point!(x: 5.0, y: 0.0)),
        ),
        (
            "endpoint",
            Line::new(point!(x: 0.0, y: 0.0), point!(x: 5.0, y: 5.0)),
            Line::new(point!(x: 5.0, y: 5.0), point!(x: 5.0, y: 0.0)),
        ),
        (
            "parallel",
            Line::new(point!(x: 0.0, y: 0.0), point!(x: 5.0, y: 5.0)),
            Line::new(point!(x: 0.0, y: 1.0), point!(x: 5.0, y: 6.0)),
        ),
        (
            "collinear",
            Line::new(point!(x: 0.0, y: 0.0), point!(x: 5.0, y: 5.0)),
            Line::new(point!(x: 3.0, y: 3.0), point!(x: 6.0, y: 6.0)),
        ),
    ];
    for (label, a, b) in cases {
        match line_intersection(a, b) {
            Some(LineIntersection::SinglePoint {
                intersection,
                is_proper,
            }) => println!(
                "x/{label} single {}:{} proper={is_proper}",
                pb(intersection.x),
                pb(intersection.y)
            ),
            Some(LineIntersection::Collinear { intersection }) => println!(
                "x/{label} collinear {}:{} -> {}:{}",
                pb(intersection.start.x),
                pb(intersection.start.y),
                pb(intersection.end.x),
                pb(intersection.end.y)
            ),
            None => println!("x/{label} none"),
        }
    }

    // ④ convex hull：定种 20 点 + 3 个对角共线点
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut pts: Vec<Point<f64>> = (0..20)
        .map(|_| point!(x: rng.coord(), y: rng.coord()))
        .collect();
    pts.push(point!(x: 25.0, y: 25.0));
    pts.push(point!(x: 50.0, y: 50.0));
    pts.push(point!(x: 75.0, y: 75.0));
    let mp = MultiPoint::new(pts);
    let hull = mp.convex_hull();
    println!(
        "hull nverts={} uarea={}",
        hull.exterior().0.len(),
        pb(hull.unsigned_area())
    );
    for (i, c) in hull.exterior().0.iter().enumerate() {
        println!("hull[{i}] {}:{}", pb(c.x), pb(c.y));
    }

    // ⑤ simplify（Douglas-Peucker）：抖动近直折线 + 一个尖刺，eps=0.1
    let ls = LineString::from(vec![
        (0.0, 0.0), (1.0, 0.05), (2.0, -0.04), (3.0, 0.03), (4.0, 0.0),
        (4.0, 3.0), (5.0, 0.02), (6.0, -0.01), (7.0, 0.0),
    ]);
    let s = ls.simplify(&0.1);
    println!("simplify n={}", s.0.len());
    for (i, c) in s.0.iter().enumerate() {
        println!("simplify[{i}] {}:{}", pb(c.x), pb(c.y));
    }

    // ⑥ BooleanOps：固定矩形 × 固定三角形，四 op
    let a = polygon![
        (x: 0.0, y: 0.0), (x: 4.0, y: 0.0), (x: 4.0, y: 4.0), (x: 0.0, y: 4.0),
    ];
    let b = polygon![
        (x: 2.0, y: 1.0), (x: 6.0, y: 3.0), (x: 2.0, y: 5.0),
    ];
    for (label, mp) in [
        ("union", a.union(&b)),
        ("intersection", a.intersection(&b)),
        ("difference", a.difference(&b)),
        ("xor", a.xor(&b)),
    ] {
        let nverts: usize = mp
            .0
            .iter()
            .map(|p| {
                p.exterior().0.len()
                    + p.interiors().iter().map(|r| r.0.len()).sum::<usize>()
            })
            .sum();
        println!(
            "bool {label} npolys={} nverts={nverts} uarea={}",
            mp.0.len(),
            pb(mp.unsigned_area())
        );
    }

    // ⑦ Haversine / Vincenty：两城市坐标对 + 精确对跖点不收敛错误路径
    //    （Vincenty 迭代对 antipodal 不收敛 → FailedToConvergeError，100 次上限）
    let beijing = point!(x: 116.4074, y: 39.9042);
    let shanghai = point!(x: 121.4737, y: 31.2304);
    let nyc = point!(x: -74.006, y: 40.7128);
    let london = point!(x: -0.1278, y: 51.5074);
    println!("hav bj-sh={}", pb(Haversine::distance(beijing, shanghai)));
    println!("hav nyc-lon={}", pb(Haversine::distance(nyc, london)));
    for (label, p, q) in [
        ("bj-sh", beijing, shanghai),
        ("nyc-lon", nyc, london),
        ("antipodal", beijing, point!(x: -63.5926, y: -39.9042)),
    ] {
        match p.vincenty_distance(&q) {
            Ok(d) => println!("vin {label} ok={}", pb(d)),
            Err(e) => println!("vin {label} err={e}"),
        }
    }
}
