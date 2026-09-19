#!/usr/bin/env mirvm
---
[dependencies]
bitvec = "1"
---
// bitvec 1: bit-level differential. Packed bit pointers (element address + bit index)
// plus strict-aliasing rules stress the mirvm place/ABI boundary. Covers:
// macro/from_slice/view_bits constructors, by-value boolean chains, split_at/splice/
// rotate/reverse, popcount and iter_ones counting, the u8/u16/u32 × le/be × Msb0/Lsb0
// conversion matrix, and BitArray plus push/pop/resize growth edges. Fixed vectors only.
use bitvec::prelude::*;

/// Bit slice -> fixed-width 01 text (iteration order, independent of storage endianness).
fn bstr<T: BitStore, O: BitOrder>(s: &BitSlice<T, O>) -> String {
    s.iter().by_vals().map(|b| if b { '1' } else { '0' }).collect()
}

fn fnv1a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Bit-field conversion matrix: u32 (le+be+sub-width) / u16 / u8, expanded per concrete
/// bit order (BitField is implemented only for the concrete Msb0/Lsb0, so O cannot be generic).
macro_rules! field_matrix {
    ($o:ty, $tag:literal) => {{
        // u32: store in both le/be directions + load from both ends
        let mut le = bitvec![u8, $o; 0; 32];
        le.store_le(0x1234_5678u32);
        println!("5 {} st_le bits={}", $tag, bstr(&le));
        println!("5 {} le-> ld_le={:08x} ld_be={:08x}", $tag, le.load_le::<u32>(), le.load_be::<u32>());
        let mut be = bitvec![u8, $o; 0; 32];
        be.store_be(0x1234_5678u32);
        println!("5 {} st_be bits={}", $tag, bstr(&be));
        println!("5 {} be-> rt={} ld_le={:08x}", $tag, be.load_be::<u32>() == 0x1234_5678, be.load_le::<u32>());
        // load from a 20-bit sub-width slice (high bits zero-extended)
        let part = &le[4..24];
        println!("5 {} part20 ld_le={:08x} ld_be={:08x}", $tag, part.load_le::<u32>(), part.load_be::<u32>());
        // u16
        let mut w16 = bitvec![u8, $o; 0; 16];
        w16.store_le(0xCAFEu16);
        println!("5 {} u16 st_le bits={}", $tag, bstr(&w16));
        println!("5 {} u16 ld_le={:04x} ld_be={:04x} rt={}", $tag, w16.load_le::<u16>(), w16.load_be::<u16>(), w16.load_le::<u16>() == 0xCAFE);
        // u8
        let mut w8 = bitvec![u8, $o; 0; 8];
        w8.store_be(0xB6u8);
        println!("5 {} u8  st_be bits={} ld_le={:02x} ld_be={:02x}", $tag, bstr(&w8), w8.load_le::<u8>(), w8.load_be::<u8>());
    }};
}

fn main() {
    // ① the three constructor families + both bit-order views
    let m = bitvec![u8, Msb0; 1, 0, 1, 1, 0, 1, 0, 0, 1, 1, 1, 0];
    println!("1 macro  len={} ones={} bits={}", m.len(), m.count_ones(), bstr(&m));
    let bytes = [0xA3u8, 0x7C, 0x82];
    let f = BitVec::<u8, Lsb0>::from_slice(&bytes);
    println!("1 fromsl len={} ones={} bits={}", f.len(), f.count_ones(), bstr(&f));
    let fm = BitVec::<u8, Msb0>::from_slice(&bytes);
    println!("1 fromsm len={} ones={} bits={}", fm.len(), fm.count_ones(), bstr(&fm));
    let host = [0xC3u8, 0x0F];
    let v = host.view_bits::<Msb0>();
    println!("1 view   len={} ones={} head13={}", v.len(), v.count_ones(), bstr(&v[..13]));
    let st = bits![u8, Msb0; 1, 1, 0, 1];
    println!("1 static len={} bits={}", st.len(), bstr(st));

    // ② by-value boolean operator chains (three 16-bit vectors)
    let a = bitvec![u8, Msb0; 1,1,0,0, 1,0,1,0, 0,0,1,1, 1,1,1,1];
    let b = bitvec![u8, Msb0; 1,0,1,0, 1,0,1,0, 1,0,1,0, 1,0,1,0];
    let c = bitvec![u8, Msb0; 0,0,0,0, 1,1,1,1, 0,1,0,1, 1,1,0,0];
    println!("2 a      bits={}", bstr(&a));
    println!("2 b      bits={}", bstr(&b));
    println!("2 c      bits={}", bstr(&c));
    let and = a.clone() & &b;
    let or = a.clone() | &b;
    let xor = a.clone() ^ &b;
    let not = !a.clone();
    println!("2 a&b    bits={} ones={}", bstr(&and), and.count_ones());
    println!("2 a|b    bits={} ones={}", bstr(&or), or.count_ones());
    println!("2 a^b    bits={} ones={}", bstr(&xor), xor.count_ones());
    println!("2 !a     bits={} ones={}", bstr(&not), not.count_ones());
    // composite chain: ((a&b) | (a^c)) ^ !c
    let chain = ((a.clone() & &b) | (a.clone() ^ &c)) ^ !c.clone();
    println!("2 chain  bits={} ones={}", bstr(&chain), chain.count_ones());
    // in-place assign-family chain
    let mut acc = a.clone();
    acc &= &b;
    acc |= &c;
    acc ^= &a;
    println!("2 assign bits={} ones={}", bstr(&acc), acc.count_ones());

    // ③ range operations: split_at / splice / rotate / reverse
    let sp = BitVec::<u8, Msb0>::from_slice(&[0b1101_0011, 0b0110_1010, 0b1010_1100]);
    let (l, r) = sp.split_at(10);
    println!("3 split  l={} r={}", bstr(l), bstr(r));
    let mut sv = BitVec::<u8, Msb0>::from_slice(&[0xFF, 0x00, 0xA5]);
    let repl = bits![u8, Msb0; 1, 0, 0, 0, 1];
    let removed: Vec<bool> = sv.splice(4..12, repl.iter().by_vals()).collect();
    let rstr: String = removed.iter().map(|&x| if x { '1' } else { '0' }).collect();
    println!("3 splice removed={} len={} now={}", rstr, sv.len(), bstr(&sv));
    let mut rot = BitVec::<u8, Msb0>::from_slice(&[0b1001_0110, 0b0011_1100]);
    rot[3..13].rotate_left(4);
    println!("3 rotl4  bits={}", bstr(&rot));
    rot[3..13].rotate_right(7);
    println!("3 rotr7  bits={}", bstr(&rot));
    let mut rev = BitVec::<u8, Lsb0>::from_slice(&[0b1011_0001, 0b0100_1110]);
    rev[2..14].reverse();
    println!("3 revseg bits={}", bstr(&rev));
    rev.reverse();
    println!("3 revall bits={}", bstr(&rev));

    // ④ popcount / iteration counts (fixed 40-bit pattern)
    let p = BitVec::<u8, Msb0>::from_slice(&[0b1110_0001, 0b0011_0111, 0b1000_0000, 0b0000_0000, 0b0111_1111]);
    println!("4 pop    len={} ones={} zeros={}", p.len(), p.count_ones(), p.count_zeros());
    println!("4 edge   lz={} lo={} tz={} to={}", p.leading_zeros(), p.leading_ones(), p.trailing_zeros(), p.trailing_ones());
    println!("4 first  one={:?} zero={:?}", p.first_one(), p.first_zero());
    println!("4 last   one={:?} zero={:?}", p.last_one(), p.last_zero());
    let ones: Vec<String> = p.iter_ones().map(|i| i.to_string()).collect();
    println!("4 iter1  [{}]", ones.join(","));
    let zeros: Vec<String> = p.iter_zeros().map(|i| i.to_string()).collect();
    println!("4 iter0  [{}]", zeros.join(","));
    let win: Vec<String> = p.chunks(8).map(|ch| ch.count_ones().to_string()).collect();
    println!("4 win8   [{}]", win.join(","));
    // backing-store byte fingerprint: 40-byte LCG (fixed seed) -> from_slice -> raw fnv
    let mut seed = 0x243F_6A88_85A3_08D3u64;
    let mut buf = [0u8; 40];
    for x in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x = (seed >> 33) as u8;
    }
    let big = BitVec::<u8, Msb0>::from_slice(&buf);
    let raw = big.as_raw_slice();
    println!("4 store  rawlen={} fnv={:016x}", raw.len(), fnv1a(raw));
    let rt = BitVec::<u8, Msb0>::from_vec(raw.to_vec());
    println!("4 store  roundtrip={} ones={}", rt.as_raw_slice() == raw, rt.count_ones());

    // ⑤ bit-field conversion matrix: u8/u16/u32 × le/be × both bit orders
    field_matrix!(Msb0, "msb");
    field_matrix!(Lsb0, "lsb");

    // ⑥ BitArray fixed length: macro / set / into_inner / booleans / new wrapping a raw array
    let mut ba = bitarr![u8, Msb0; 1,0,1,0, 0,1,0,1, 1,1,0,0, 0,0,1,1];
    println!("6 arr    len={} bits={}", ba.len(), bstr(&ba));
    ba.set(0, false);
    ba.set(9, true);
    println!("6 set    bits={}", bstr(&ba));
    let inner: [u8; 2] = ba.into_inner();
    println!("6 inner  hex={:02x}{:02x}", inner[0], inner[1]);
    let bw = BitArray::<[u32; 2], Lsb0>::new([0xDEAD_BEEF, 0x0F0F_F0F0]);
    println!("6 wrap   len={} ones={} head16={}", bw.len(), bw.count_ones(), bstr(&bw[..16]));
    let bx = bw ^ BitArray::<[u32; 2], Lsb0>::new([0x0000_FFFF, 0x00FF_00FF]);
    println!("6 xor    ones={} head16={}", bx.count_ones(), bstr(&bx[..16]));
    let bn = !bw;
    println!("6 not    ones={}", bn.count_ones());
    let raw32: [u32; 2] = bx.into_inner();
    println!("6 raw32  {:08x},{:08x}", raw32[0], raw32[1]);

    // ⑦ growth and edges: empty vector, push/pop, resize, split_at(0), non-byte-aligned tail
    let mut g = BitVec::<u8, Msb0>::new();
    println!("7 empty  len={} ones={} bits=[{}]", g.len(), g.count_ones(), bstr(&g));
    for i in 0..17 {
        g.push(i % 3 == 0);
    }
    println!("7 push   len={} bits={}", g.len(), bstr(&g));
    let popped: Vec<String> = (0..3).map(|_| g.pop().unwrap().to_string()).collect();
    println!("7 pop    [{}] len={} bits={}", popped.join(","), g.len(), bstr(&g));
    g.resize(20, true);
    println!("7 resize len={} bits={}", g.len(), bstr(&g));
    let (e1, e2) = g.split_at(0);
    println!("7 split0 l=[{}] rlen={}", bstr(e1), e2.len());
    let tail = &g[15..];
    println!("7 tail5  bits={} ones={}", bstr(tail), tail.count_ones());
}
