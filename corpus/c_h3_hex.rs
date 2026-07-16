#!/usr/bin/env mirvm
---
[dependencies]
h3o = { version = "0.8", features = ["geo"] }
geo = "0.30"
---
// h3o 0.8（Uber H3 六边形网格索引纯 Rust 实现）差分。h3o 的 lat/lng→cell 转换是
// 重度三角函数路径（gnomonic 投影 + face 旋转 + IJK 量化），cell boundary/edge
// length 同理——全链路 f64 sin/cos/atan2/asin/acos/tan/mul_add 逐位对拍，
// 任何 native/mirvm 输出差异即 miscompile 候选。
// 依赖树（default+geo）：h3o-bit / either / float_eq / libm / ahash(std,
// compile-time-rng——编译期定钥，无运行期 OS 熵源) / geo 0.30 纯 Rust。
// Tiler 内部 ahash::HashSet 只做 dedup（insert 语义与哈希无关），coverage 输出
// 序为 Vec 遍历序——确定性不依赖哈希种子。grid_disk 走 VecDeque BFS，HashSet 亦
// 只做成员判断，输出序即 API 遍历序，直接按 API 序打印。
//
// 覆盖：
// ① 固定坐标集 8 点（含近极点/对蹼子午线两侧）× res 0/5/9/15 → to_cell，
//    打印 index u64 bits / hex Display / pentagon 位；res9 追加 area_km2、
//    boundary 逐顶点 lat/lng、icosahedron_faces、direction_at 全谱。
// ② 邻接：hex 与 pentagon 两种 cell 的 grid_disk k=1/2（API 序 join）、
//    grid_disk_distances(k=2)、pentagon 的 grid_disk_fast/grid_ring_fast
//    （Option 空洞形态）。
// ③ 层级：parent 链 / parent(过细 res)=None / parent(同 res)=self /
//    children + children_count（hex 与 pentagon 两族）/ center_child /
//    child_position / child_at 正反解 / children(更粗 res)=空迭代。
// ④ 距离与 line：grid_distance 双向 / grid_path_cells_size / grid_path_cells
//    全路径 / to_local_ij + TryFrom<LocalIJ> 反解 / is_neighbor_with 三态 /
//    edges×6（DirectedEdgeIndex bits + length_km + destination roundtrip）/
//    vertexes×6（VertexIndex bits + owner）。
// ⑤ polygon containment（geo feature）：TilerBuilder × 四种 ContainmentMode
//    固定凸多边形 res8 coverage（排序后 fnv+全 join）、coverage_size_hint；
//    NaN 顶点 → InvalidGeometry 错误路径。
// ⑥ 错误路径：NaN/Inf 坐标、非法 u64/hex 串 cell index、Resolution=16、
//    Direction=7、Edge=0、Vertex=6、compact 重复/混分辨率、grid_distance
//    与 grid_path_cells 的 ResolutionMismatch。
// ⑦ compact/uncompact roundtrip：res2 cell 的 49 个 res4 子 cell 压实回
//    单个 res2 parent 再展开计数。
//
// 确定性：坐标/多边形全部常量；无随机；浮点一律 to_bits() 锁位型打印；
// 无 HashMap 迭代、无地址/时间/线程序；成功路径 stderr 为空。
use std::str::FromStr;

use geo::{LineString, Polygon};
use h3o::geom::{ContainmentMode, TilerBuilder};
use h3o::{CellIndex, DirectedEdgeIndex, Direction, Edge, LatLng, Resolution, Vertex};

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 一组 cell 的 u64 bits 流指纹。
fn cells_fnv(cells: &[CellIndex]) -> u64 {
    let mut bytes = Vec::with_capacity(cells.len() * 8);
    for c in cells {
        bytes.extend_from_slice(&u64::from(*c).to_le_bytes());
    }
    fnv1a(&bytes)
}

fn join_cells(cells: &[CellIndex]) -> String {
    cells
        .iter()
        .map(|c| format!("{c:x}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// f64 → 锁位十六进制。
fn b(x: f64) -> String {
    format!("{:016x}", x.to_bits())
}

/// Option<cell>（fast 族空洞）→ hex 或 "-"。
fn opt(cell: Option<CellIndex>) -> String {
    cell.map(|c| format!("{c:x}")).unwrap_or_else(|| "-".to_string())
}

fn main() {
    // ===== ① 固定坐标集 × res 0/5/9/15 → cell =====
    let coords: [(f64, f64); 8] = [
        (48.864716, 2.349014),      // 巴黎
        (31.230416, 121.473701),    // 上海
        (30.243684, 120.149963),    // 西湖
        (-33.868820, 151.209296),   // 悉尼（南半球）
        (89.9, 45.0),               // 近北极点
        (-89.9, -120.0),            // 近南极点
        (0.0, 179.9999),            // 对蹼子午线东侧
        (0.0, -179.9999),           // 对蹼子午线西侧
    ];
    let mut at9 = Vec::new();
    for (i, &(lat, lng)) in coords.iter().enumerate() {
        let ll = LatLng::new(lat, lng).unwrap();
        println!("ll{i} lat={} lng={}", b(ll.lat()), b(ll.lng()));
        for res in [Resolution::Zero, Resolution::Five, Resolution::Nine, Resolution::Fifteen] {
            let cell = ll.to_cell(res);
            println!(
                "c{i}/r{} bits={:016x} hex={cell:x} pent={}",
                u8::from(res),
                u64::from(cell),
                cell.is_pentagon()
            );
            if res == Resolution::Nine {
                at9.push(cell);
            }
        }
    }
    // res9 浮点重路径：面积 + boundary 逐顶点 + 所属二十面体面 + 方向谱
    let p9 = at9[0];
    let s9 = at9[1];
    println!(
        "p9 area_km2={} area_m2={} max_faces={}",
        b(p9.area_km2()),
        b(p9.area_m2()),
        p9.max_face_count()
    );
    let faces = p9.icosahedron_faces();
    println!(
        "p9 faces len={} set={:?}",
        faces.len(),
        faces.iter().map(u8::from).collect::<Vec<_>>()
    );
    for (i, ll) in p9.boundary().iter().enumerate() {
        println!("p9 boundary[{i}] lat={} lng={}", b(ll.lat_radians()), b(ll.lng_radians()));
    }
    for res in Resolution::range(Resolution::One, Resolution::Ten) {
        let d = p9.direction_at(res).map(u8::from);
        println!("p9 dir@{}={:?}", u8::from(res), d);
    }
    println!(
        "p9 dir@0={:?} base_cell={} base_cells={}",
        p9.direction_at(Resolution::Zero).map(u8::from),
        u8::from(p9.base_cell()),
        CellIndex::base_cells().count()
    );
    // pentagon 谱（res9 共 12 个）+ 其 boundary（5 顶点）
    let mut pents9: Vec<CellIndex> = Resolution::Nine.pentagons().collect();
    pents9.sort();
    println!("pentagons9 n={} {}", pents9.len(), join_cells(&pents9));
    let pent9 = pents9[0];
    println!(
        "pent9 boundary n={} fnv={:016x}",
        pent9.boundary().len(),
        fnv1a(
            &pent9
                .boundary()
                .iter()
                .flat_map(|ll| [ll.lat_radians().to_bits(), ll.lng_radians().to_bits()])
                .flat_map(u64::to_le_bytes)
                .collect::<Vec<_>>()
        )
    );

    // ===== ② 邻接 k-ring =====
    let ring1: Vec<CellIndex> = p9.grid_disk(1);
    println!("p9 disk1 n={} {}", ring1.len(), join_cells(&ring1));
    let ring2: Vec<CellIndex> = p9.grid_disk(2);
    println!("p9 disk2 n={} fnv={:016x}", ring2.len(), cells_fnv(&ring2));
    println!("p9 disk2 {}", join_cells(&ring2));
    let dd2: Vec<(CellIndex, u32)> = p9.grid_disk_distances(2);
    println!(
        "p9 disk_dist2 n={} {}",
        dd2.len(),
        dd2.iter()
            .map(|(c, d)| format!("{c:x}:{d}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    // pentagon：k=1 磁盘缺 1 邻（6 cell），k=2 缺 2（17 cell）
    let pdisk1: Vec<CellIndex> = pent9.grid_disk(1);
    println!("pent9 disk1 n={} {}", pdisk1.len(), join_cells(&pdisk1));
    let pdisk2: Vec<CellIndex> = pent9.grid_disk(2);
    println!("pent9 disk2 n={} fnv={:016x}", pdisk2.len(), cells_fnv(&pdisk2));
    println!("pent9 disk2 {}", join_cells(&pdisk2));
    // fast 族：Option 空洞（Class III / pentagon 越界位置）
    let pfast1: Vec<Option<CellIndex>> = pent9.grid_disk_fast(1).collect();
    println!(
        "pent9 disk_fast1 n={} {}",
        pfast1.len(),
        pfast1.iter().map(|&o| opt(o)).collect::<Vec<_>>().join(",")
    );
    let pring2: Vec<Option<CellIndex>> = pent9.grid_ring_fast(2).collect();
    println!(
        "pent9 ring_fast2 n={} {}",
        pring2.len(),
        pring2.iter().map(|&o| opt(o)).collect::<Vec<_>>().join(",")
    );
    let hring2: Vec<Option<CellIndex>> = p9.grid_ring_fast(2).collect();
    println!(
        "p9 ring_fast2 n={} {}",
        hring2.len(),
        hring2.iter().map(|&o| opt(o)).collect::<Vec<_>>().join(",")
    );
    println!(
        "max_disk_size k0={} k2={}",
        h3o::max_grid_disk_size(0),
        h3o::max_grid_disk_size(2)
    );

    // ===== ③ 层级 parent/children =====
    for res in [
        Resolution::Eight,
        Resolution::Five,
        Resolution::Two,
        Resolution::Zero,
    ] {
        println!(
            "p9 parent@{}={}",
            u8::from(res),
            opt(p9.parent(res))
        );
    }
    println!(
        "p9 parent@12={:?} parent@9_eq_self={}",
        p9.parent(Resolution::Twelve).map(|c| format!("{c:x}")),
        p9.parent(Resolution::Nine) == Some(p9)
    );
    println!(
        "p9 center_child@15={} center_child@3={:?}",
        opt(p9.center_child(Resolution::Fifteen)),
        p9.center_child(Resolution::Three).map(|c| format!("{c:x}"))
    );
    println!(
        "children_count p9@15={} pent9@15={} p9@9={}",
        p9.children_count(Resolution::Fifteen),
        pent9.children_count(Resolution::Fifteen),
        p9.children_count(Resolution::Nine)
    );
    let kids: Vec<CellIndex> = p9.parent(Resolution::Two).unwrap().children(Resolution::Four).collect();
    println!(
        "p2 children@4 n={} first={} last={} fnv={:016x}",
        kids.len(),
        kids[0],
        kids[kids.len() - 1],
        cells_fnv(&kids)
    );
    println!(
        "p9 children@3 n={}",
        p9.children(Resolution::Three).count()
    );
    // child_position / child_at 正反解（doc 用例原值）
    let doccell = CellIndex::try_from(0x8a1fb46622dffff).unwrap();
    println!(
        "doc child_position@8={:?} @12={:?}",
        doccell.child_position(Resolution::Eight),
        doccell.child_position(Resolution::Twelve)
    );
    let dparent = doccell.parent(Resolution::Eight).unwrap();
    println!(
        "doc child_at(24,10)={} child_at(24,5)={:?} child_at(oob)={:?}",
        dparent.child_at(24, Resolution::Ten) == Some(doccell),
        dparent.child_at(24, Resolution::Five).map(|c| format!("{c:x}")),
        dparent
            .child_at(dparent.children_count(Resolution::Twelve), Resolution::Twelve)
            .map(|c| format!("{c:x}"))
    );

    // ===== ④ 距离与 line =====
    let paris = LatLng::new(48.864716, 2.349014).unwrap();
    let london = LatLng::new(51.507222, -0.1275).unwrap();
    println!(
        "ll dist paris→london rads={} km={} m={}",
        b(paris.distance_rads(london)),
        b(paris.distance_km(london)),
        b(paris.distance_m(london))
    );
    let b9 = london.to_cell(Resolution::Nine);
    println!(
        "grid_distance p9→b9={} b9→p9={} size={}",
        p9.grid_distance(b9).unwrap(),
        b9.grid_distance(p9).unwrap(),
        p9.grid_path_cells_size(b9).unwrap()
    );
    let path: Vec<CellIndex> = p9
        .grid_path_cells(b9)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    println!("path n={} fnv={:016x}", path.len(), cells_fnv(&path));
    println!("path {}", join_cells(&path));
    let back: Vec<CellIndex> = b9
        .grid_path_cells(p9)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    println!("path_rev n={} eq={}", back.len(), {
        let mut r = back.clone();
        r.reverse();
        r == path
    });
    // local IJ + 反解
    let lij = b9.to_local_ij(p9).unwrap();
    println!(
        "local_ij anchor={} i={} j={} roundtrip={}",
        lij.anchor,
        lij.coord.i,
        lij.coord.j,
        CellIndex::try_from(lij) == Ok(b9)
    );
    // is_neighbor_with 三态
    let nb1: Vec<CellIndex> = p9.grid_disk(1);
    println!(
        "neighbor direct={} far={} ",
        p9.is_neighbor_with(nb1[1]).unwrap(),
        p9.is_neighbor_with(b9).unwrap()
    );
    match p9.is_neighbor_with(p9.parent(Resolution::Eight).unwrap()) {
        Ok(v) => println!("neighbor mismatch ok={v}"),
        Err(e) => println!("neighbor mismatch err: {e}"),
    }
    // DirectedEdge × 6
    println!(
        "edge direct={} nonedge={:?}",
        p9.edge(nb1[1]).is_some(),
        p9.edge(b9).map(|e| format!("{e:x}"))
    );
    for e in p9.edges() {
        println!(
            "p9 edge={:x} bits={:016x} len_km={} origin={:x} dest={:x} rt={}",
            e,
            u64::from(e),
            b(e.length_km()),
            u64::from(e.origin()),
            u64::from(e.destination()),
            DirectedEdgeIndex::try_from(u64::from(e)) == Ok(e)
        );
    }
    for v in p9.vertexes() {
        println!("p9 vertex={v:x} bits={:016x} owner={:x}", u64::from(v), u64::from(v.owner()));
    }
    println!(
        "pent9 edges={} vertexes={}",
        pent9.edges().count(),
        pent9.vertexes().count()
    );

    // ===== ⑤ polygon containment（Tiler × 4 mode）=====
    let poly = Polygon::new(
        LineString::from(vec![
            (2.300000, 48.840000),
            (2.400000, 48.840000),
            (2.400000, 48.890000),
            (2.300000, 48.890000),
            (2.300000, 48.840000),
        ]),
        vec![],
    );
    for mode in [
        ContainmentMode::ContainsCentroid,
        ContainmentMode::ContainsBoundary,
        ContainmentMode::IntersectsBoundary,
        ContainmentMode::Covers,
    ] {
        let mut tiler = TilerBuilder::new(Resolution::Eight)
            .containment_mode(mode)
            .build();
        tiler.add(poly.clone()).unwrap();
        let hint = tiler.coverage_size_hint();
        let mut cov: Vec<CellIndex> = tiler.into_coverage().collect();
        cov.sort();
        cov.dedup();
        println!(
            "coverage {mode:?} hint={hint} n={} fnv={:016x}",
            cov.len(),
            cells_fnv(&cov)
        );
        println!("coverage {mode:?} {}", join_cells(&cov));
    }
    // 错误路径：NaN 顶点 → InvalidGeometry
    let bad_poly = Polygon::new(
        LineString::from(vec![
            (f64::NAN, 0.0),
            (1.0, 0.0),
            (1.0, 1.0),
            (f64::NAN, 0.0),
        ]),
        vec![],
    );
    let mut tbad = TilerBuilder::new(Resolution::Eight).build();
    match tbad.add(bad_poly) {
        Ok(()) => println!("bad_poly unexpectedly ok"),
        Err(e) => println!("bad_poly err: {e}"),
    }

    // ===== ⑥ 无效坐标 / 非法 index / 错误路径 =====
    println!(
        "latlng nan={:?} inf={:?}",
        LatLng::new(f64::NAN, 10.0).map(|_| "ok"),
        LatLng::new(10.0, f64::NEG_INFINITY).map(|_| "ok")
    );
    for bits in [0u64, 0xffffffffffffffff, 0x8f1fb46622d8001] {
        println!("cell from_bits {bits:016x} = {:?}", CellIndex::try_from(bits).map(|c| format!("{c:x}")));
    }
    println!(
        "cell fromstr ok={:?} badhex={:?} badbits={:?}",
        CellIndex::from_str("8f1fb46622d8000").map(|c| format!("{c:x}")),
        CellIndex::from_str("zzzz").map(|c| format!("{c:x}")),
        CellIndex::from_str("fffffffffffffff").map(|c| format!("{c:x}"))
    );
    println!(
        "resolution 16={:?} direction 7={:?} edge 0={:?} vertex 6={:?}",
        Resolution::try_from(16u8).map(u8::from),
        Direction::try_from(7u8).map(u8::from),
        Edge::try_from(0u8).map(u8::from),
        Vertex::try_from(6u8).map(u8::from)
    );
    match p9.grid_distance(p9.parent(Resolution::Five).unwrap()) {
        Ok(v) => println!("grid_distance mismatch ok={v}"),
        Err(e) => println!("grid_distance mismatch err: {e}"),
    }
    match p9.grid_path_cells(p9.parent(Resolution::Five).unwrap()) {
        Ok(_) => println!("grid_path mismatch unexpectedly ok"),
        Err(e) => println!("grid_path mismatch err: {e}"),
    }
    // compact 错误面：重复 cell / 混分辨率
    let dup = vec![ring1[0], ring1[0]];
    match CellIndex::compact(&mut dup.clone()) {
        Ok(()) => println!("compact dup unexpectedly ok"),
        Err(e) => println!("compact dup err: {e:?}"),
    }
    let mut mixed = vec![p9, p9.parent(Resolution::Five).unwrap()];
    match CellIndex::compact(&mut mixed) {
        Ok(()) => println!("compact mixed unexpectedly ok"),
        Err(e) => println!("compact mixed err: {e:?}"),
    }

    // ===== ⑦ compact / uncompact roundtrip =====
    let p2 = p9.parent(Resolution::Two).unwrap();
    let mut full: Vec<CellIndex> = p2.children(Resolution::Four).collect();
    let n_before = full.len();
    CellIndex::compact(&mut full).unwrap();
    println!(
        "compact {n_before}→{} [{}]",
        full.len(),
        join_cells(&full)
    );
    println!(
        "uncompact_size={} uncompact n={} roundtrip={}",
        CellIndex::uncompact_size(full.iter().copied(), Resolution::Four),
        CellIndex::uncompact(full.iter().copied(), Resolution::Four).count(),
        CellIndex::uncompact(full.iter().copied(), Resolution::Four).eq(kids.iter().copied())
    );
    // 上海/悉尼 cell 的超远距离：跨 base cell「cannot unfold」为 H3 正常错误路径
    let y9 = at9[3];
    println!(
        "grid_distance p9→s9={:?} p9→y9={:?}",
        p9.grid_distance(s9),
        p9.grid_distance(y9)
    );
}
