// M5.2 D8b permanent differential probe for the full simd family: LaneKind semantic boundary vs. native on the same machine.
// Focuses on the "formerly silent wrong-value surface": float lane compare/arithmetic is not bitwise (+0.0==−0.0,
// NaN is not reflexive, NaN propagation), signed lane division/shift/saturation boundaries, masked memory must not fake reads/writes.
// Surfaces with no public API (funnel/masked/dyn lane/arith_offset) call core intrinsics directly
// -- native is compiled by the same pinned rustc, so same-source differential comparison holds.
#![feature(portable_simd, core_intrinsics)]
#![allow(internal_features)]

use std::intrinsics::simd as si;
use std::simd::StdFloat;
use std::simd::prelude::*;

fn float_lanes() {
    let z = f32x4::from_array([0.0, -0.0, f32::NAN, f32::INFINITY]);
    let y = f32x4::from_array([-0.0, 0.0, f32::NAN, 1.0e30]);
    // Bitwise compare would deem +0.0/−0.0 unequal and NaN equal -- both are wrong.
    println!("f eq/ne = {:?} {:?}", z.simd_eq(y).to_array(), z.simd_ne(y).to_array());
    println!("f lt/ge = {:?} {:?}", z.simd_lt(y).to_array(), z.simd_ge(y).to_array());
    let s = z + y;
    println!("f add nan/inf = {:?} {}", s.is_nan().to_array(), s[3]);
    let d = f64x2::from_array([1.0, -7.5]) / f64x2::from_array([0.0, 2.5]);
    println!("f div = {} {}", d[0], d[1]); // inf、-3
    let m = f64x2::from_array([5.0, f64::NAN]);
    let n = f64x2::from_array([3.0, 2.0]);
    println!("f min/max = {:?} {:?}", m.simd_min(n).to_array(), m.simd_max(n).to_array());
    println!("f neg/abs = {:?} {:?}",
        (-f32x4::splat(1.5)).to_array(), f32x4::from_array([-2.0, 3.0, -0.0, 4.5]).abs().to_array());
    println!("f sqrt/floor/ceil = {:?} {:?} {:?}",
        f64x2::from_array([2.0, 9.0]).sqrt().to_array(),
        f32x4::splat(2.7).floor().to_array(), f32x4::splat(2.1).ceil().to_array());
    let t = f64x2::from_array([0.5, 1.0]);
    println!("f sin/exp/ln = {:?} {:?} {:?}",
        t.sin().to_array(), t.exp().to_array(), t.ln().to_array());
    println!("f fma = {:?}",
        f32x4::splat(1.5).mul_add(f32x4::splat(2.0), f32x4::splat(-1.0)).to_array());
    // reduce: ordered fold vs. unordered allowed set (ordered is always in the set)
    let r = f32x4::from_array([1.5, 2.5, 3.5, 4.5]);
    println!("f reduce sum/mul/min/max = {} {} {} {}",
        r.reduce_sum(), r.reduce_product(), r.reduce_min(), r.reduce_max());
}

fn int_lanes() {
    let a = i8x8::from_array([i8::MIN, -1, 0, 1, i8::MAX, 100, -100, 7]);
    let b = i8x8::from_array([1, 2, -3, 4, 1, 100, -100, -2]);
    println!("i mul = {:?}", (a * b).to_array()); // wrapping
    println!("i sat add/sub = {:?} {:?}",
        a.saturating_add(b).to_array(), a.saturating_sub(b).to_array());
    let c = i32x4::from_array([-7, 7, i32::MIN + 1, 100]);
    let d = i32x4::from_array([2, -2, 3, 7]);
    println!("i div/rem = {:?} {:?}", (c / d).to_array(), (c % d).to_array());
    let u = u16x4::from_array([0xF00F, 1, 0x8000, u16::MAX]);
    println!("u shl/shr = {:?} {:?}",
        (u << u16x4::splat(4)).to_array(), (u >> u16x4::splat(3)).to_array());
    let sh = i16x4::from_array([-32768, -2, 4, -1]);
    println!("i shr arith = {:?}", (sh >> i16x4::splat(2)).to_array());
    println!("i ct = {:?} {:?} {:?}",
        u.leading_zeros().to_array(), u.trailing_zeros().to_array(), u.count_ones().to_array());
    println!("i bswap/brev = {:?} {:?}",
        u.swap_bytes().to_array(), u.reverse_bits().to_array());
    println!("i reduce = {} {} {} {} {}",
        a.reduce_sum(), a.reduce_product(), a.reduce_min(), a.reduce_max(),
        u.reduce_and());
    // funnel (no public API): concatenated-window shift
    let fa = u32x4::from_array([0xDEAD_BEEF, 1, 0x8000_0000, 0xFFFF_FFFF]);
    let fb = u32x4::from_array([0x1234_5678, 0x8000_0000, 0, 1]);
    let fs = u32x4::from_array([8, 1, 31, 0]);
    let (l, r): (u32x4, u32x4) = unsafe {
        (si::simd_funnel_shl(fa, fb, fs), si::simd_funnel_shr(fa, fb, fs))
    };
    println!("funnel = {:?} {:?}", l.to_array(), r.to_array());
}

fn casts() {
    let f = f32x4::from_array([1.9, -1.9, 3.0e9, f32::NAN]);
    println!("f->i sat = {:?} {:?}",
        f.cast::<i32>().to_array(), f.cast::<u8>().to_array());
    let i = i8x4::from_array([-1, 127, -128, 5]);
    println!("i widen = {:?} {:?}",
        i.cast::<i32>().to_array(), i.cast::<u16>().to_array());
    let w = i32x4::from_array([300, -300, 128, -129]);
    println!("i narrow = {:?}", w.cast::<i8>().to_array());
    println!("i->f = {:?}", i.cast::<f64>().to_array());
    println!("f64->f32 = {:?}", f64x2::from_array([1.0e300, -2.5]).cast::<f32>().to_array());
}

fn memory_lanes() {
    // gather/scatter: dummy-lane pointers point to a sentinel; fake reads/writes would corrupt the sentinel value
    let data = [10i32, 20, 30, 40];
    let sentinel = Box::new(-1i32);
    let ptrs = Simd::<*const i32, 4>::from_array([
        &data[3], &*sentinel, &data[1], &data[0],
    ]);
    let mask = i32x4::from_array([-1, 0, -1, 0]);
    let pass = i32x4::from_array([7, 8, 9, 11]);
    let g: i32x4 = unsafe { si::simd_gather(pass, ptrs, mask) };
    println!("gather = {:?}", g.to_array());
    let mut out = [0i32; 4];
    let wptrs = Simd::<*mut i32, 4>::from_array([
        &mut out[0], &mut out[1], &mut out[2], &mut out[3],
    ]);
    unsafe { si::simd_scatter(i32x4::from_array([5, 6, 7, 8]), wptrs, mask) };
    println!("scatter = {out:?} sentinel = {sentinel}");

    let buf = [1u16, 2, 3, 4, 5, 6, 7, 8];
    let m8 = i16x8::from_array([-1, -1, 0, -1, 0, 0, -1, 0]);
    let ml: u16x8 = unsafe {
        si::simd_masked_load::<_, _, _, { si::SimdAlign::Element }>(
            m8, buf.as_ptr(), u16x8::splat(99),
        )
    };
    println!("masked_load = {:?}", ml.to_array());
    let mut mout = [0u16; 8];
    unsafe {
        si::simd_masked_store::<_, _, _, { si::SimdAlign::Element }>(
            m8, mout.as_mut_ptr(), ml,
        )
    };
    println!("masked_store = {mout:?}");

    // dynamic lane
    let v = u32x4::from_array([11, 22, 33, 44]);
    let e: u32 = unsafe { si::simd_extract_dyn(v, 2) };
    let v2: u32x4 = unsafe { si::simd_insert_dyn(v, 1, 99u32) };
    println!("dyn = {e} {:?}", v2.to_array());

    // arith_offset: pointer lane offset element-by-element
    let base = Simd::<*const i32, 4>::from_array([&data[0]; 4]);
    let off = Simd::<isize, 4>::from_array([0, 1, 2, 3]);
    let stepped: Simd<*const i32, 4> = unsafe { si::simd_arith_offset(base, off) };
    let vals: [i32; 4] = std::array::from_fn(|i| unsafe { *stepped[i] });
    println!("arith_offset = {vals:?}");

    // select_bitmask
    let sel: i32x4 = unsafe {
        si::simd_select_bitmask(0b0101u8, i32x4::splat(1), i32x4::splat(0))
    };
    println!("select_bitmask = {:?}", sel.to_array());
}

fn main() {
    float_lanes();
    int_lanes();
    casts();
    memory_lanes();
}
