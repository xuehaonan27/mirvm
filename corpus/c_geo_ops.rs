#!/usr/bin/env mirvm
---
[dependencies]
# default-features = false: the default multithreading feature pulls rayon into
# i_overlay's parallel BooleanOps merge, and the differential wants the smallest
# possible scheduling surface (float merges on the cooperative scheduler carry no
# extra signal). Every API this fixture targets is on the core path: area/centroid/
# contains/coordinate_position/line_intersection/convex_hull/simplify/BooleanOps.
geo = { version = "0.29", default-features = false }
---
// geo 0.29: heavy differential for floating-point geographic geometry. Coverage:
// ① Polygon unsigned/signed area, centroid (including the empty-polygon None path) and
//    perimeter. geo 0.29 has no Perimeter trait, so the perimeter is the hand-computed
//    sum of the exterior and interior Euclidean lengths; holed polygons are included.
// ② Point-in-polygon: positive and negative contains cases plus the three-way
//    CoordinatePosition breakdown (inside/outside/in a hole/on an edge/at a vertex).
// ③ The four line_intersection shapes: proper SinglePoint, endpoint non-proper,
//    parallel None and collinear.
// ④ Convex hull (quick_hull) over a seeded xorshift point set plus diagonal collinear points.
// ⑤ Douglas-Peucker simplify with a fixed epsilon of 0.1 (jittered polyline and a spike).
// ⑥ BooleanOps: union/intersection/difference/xor of two fixed polygons, printing the
//    sub-polygon count, total vertex count and result area bits.
// ⑦ Haversine (the line_measures API, Haversine::distance) and Vincenty distances for
//    two city coordinate pairs plus the non-convergent near-antipodal error path.
// geo 0.29 has no buffer/offset API, so that is skipped; deterministic throughout:
// floats print as to_bits hex, the point set is seeded, and there is no HashMap
// iteration, time or address dependence.
use geo::coordinate_position::CoordPos;
use geo::line_intersection::{line_intersection, LineIntersection};
use geo::{
    point, polygon, Area, BooleanOps, Centroid, Contains, ConvexHull, CoordinatePosition,
    Distance, Euclidean, Haversine, Length, Line, LineString, MultiPoint, Point, Polygon,
    Simplify, VincentyDistance,
};

/// Seeded xorshift64*, the same sequence on native and mirvm.
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

    /// Pseudo-random coordinate on a 1/16 grid in [0,100), by integer modulo, bit-exact.
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

/// Perimeter = sum of the Euclidean lengths of the exterior and interiors (geo 0.29 has no Perimeter trait).
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
    // ① area/centroid/perimeter: a simple pentagon, a holed rectangle and an empty polygon (None path)
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

    // ② point-in-polygon: positive and negative contains cases plus the three-way position breakdown
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

    // ③ the four line_intersection shapes
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

    // ④ convex hull: 20 seeded points plus 3 diagonal collinear points
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

    // ⑤ simplify (Douglas-Peucker): a jittered near-straight polyline plus one spike, eps=0.1
    let ls = LineString::from(vec![
        (0.0, 0.0), (1.0, 0.05), (2.0, -0.04), (3.0, 0.03), (4.0, 0.0),
        (4.0, 3.0), (5.0, 0.02), (6.0, -0.01), (7.0, 0.0),
    ]);
    let s = ls.simplify(&0.1);
    println!("simplify n={}", s.0.len());
    for (i, c) in s.0.iter().enumerate() {
        println!("simplify[{i}] {}:{}", pb(c.x), pb(c.y));
    }

    // ⑥ BooleanOps: a fixed rectangle x a fixed triangle, four operations
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

    // ⑦ Haversine / Vincenty: two city coordinate pairs plus the exactly antipodal
    //    non-convergence error path (Vincenty's iteration fails to converge within 100 rounds).
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
