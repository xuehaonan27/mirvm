// M5.2 D8i permanent differential probe for the scalar-intrinsic delta set: fabs generic name (drifts on this nightly),
// four variants of atomic fetch_max/min, fma/fmuladd, fast/algebraic floats, volatile bulk memory access,
// nontemporal_store, ptr_mask, vtable_size/align, nullary type queries -- compared bit-for-bit with native on the same machine.
// breakpoint is not here (SIGTRAP terminates the process; cannot continue differential run).
#![feature(core_intrinsics, ptr_metadata, variant_count)]
#![allow(internal_features)]

use std::sync::atomic::{AtomicI8, AtomicI64, AtomicU8, AtomicU64, Ordering};

fn float_math() {
    // fabs：本 nightly 泛型名 intrinsic（M5.2 前 mirvm 一调即 Trap 的暗洞）
    println!("abs = {} {} {}", (-3.5f64).abs(), (-2.25f32).abs(), (-0.0f64).abs());
    println!("signum/max/copysign = {} {} {}",
        (-7.0f64).signum(), 1.5f32.max(2.5), 3.0f64.copysign(-1.0));
    // fma 融合证明：x*x + (-(x*x)) 融合时非零（单次舍入留尾），非融合恒零
    let x64 = 1.0f64 + (-30f64).exp2();
    let x32 = 1.0f32 + (-12f32).exp2();
    println!("fma fused = {:e} {:e}", x64.mul_add(x64, -(x64 * x64)),
        x32.mul_add(x32, -(x32 * x32)));
    println!("fma plain = {} {}", 2.0f64.mul_add(3.0, 1.0), 2.0f32.mul_add(0.5, -1.0));
    // fast/algebraic：按精确语义执行在允许集合内（native 无重结合机会时同值）
    unsafe {
        use std::intrinsics::{fadd_fast, fdiv_algebraic, fmul_algebraic, frem_algebraic};
        println!("fast = {} {} {} {}",
            fadd_fast(1.25f64, 2.5), fmul_algebraic(3.0f32, 1.5),
            fdiv_algebraic(7.0f64, 2.0), frem_algebraic(7.5f64, 2.0));
    }
}

fn atomic_minmax() {
    let a = AtomicI64::new(5);
    let old1 = a.fetch_max(9, Ordering::SeqCst);
    let old2 = a.fetch_min(-3, Ordering::SeqCst);
    let b = AtomicI8::new(-128);
    let old3 = b.fetch_max(127, Ordering::SeqCst);
    let c = AtomicU8::new(200);
    let old4 = c.fetch_max(255, Ordering::SeqCst);
    let old5 = c.fetch_min(10, Ordering::SeqCst);
    let d = AtomicU64::new(u64::MAX);
    let old6 = d.fetch_min(7, Ordering::SeqCst);
    println!("fetch_max/min olds = {old1} {old2} {old3} {old4} {old5} {old6}");
    println!("fetch_max/min finals = {} {} {} {}",
        a.load(Ordering::SeqCst), b.load(Ordering::SeqCst),
        c.load(Ordering::SeqCst), d.load(Ordering::SeqCst));
}

fn volatile_mem() {
    use std::intrinsics::{volatile_copy_memory, volatile_copy_nonoverlapping_memory,
        volatile_set_memory};
    let mut buf: [u16; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    unsafe {
        // 重叠拷贝（memmove 语义）：往后错 2 个元素
        volatile_copy_memory(buf.as_mut_ptr().add(2), buf.as_ptr(), 6);
        println!("vol overlap = {buf:?}");
        let mut dst: [u16; 4] = [0; 4];
        volatile_copy_nonoverlapping_memory(dst.as_mut_ptr(), buf.as_ptr().add(1), 4);
        println!("vol copy = {dst:?}");
        volatile_set_memory(dst.as_mut_ptr(), 0xAB, 2); // elem=u16 → 4 字节
        println!("vol set = {dst:?}");
    }
}

fn nt_and_mask() {
    let mut cell: u64 = 0;
    let mut wide: [u8; 16] = [0; 16];
    unsafe {
        std::intrinsics::nontemporal_store(&mut cell, 0xDEAD_BEEF_u64);
        std::intrinsics::nontemporal_store(&mut wide, *b"nontemporal_16b!");
    }
    println!("nt = {cell:#x} {}", std::str::from_utf8(&wide).unwrap());

    #[repr(align(64))]
    struct Aligned([u8; 64]);
    let a = Aligned(std::array::from_fn(|i| i as u8));
    let p = unsafe { a.0.as_ptr().add(21) };
    let masked = std::intrinsics::ptr_mask(p, !15usize); // 抹到 16 对齐 → 元素 16
    println!("ptr_mask = {} {}", unsafe { *masked }, p as usize - masked as usize);
}

trait Speak {
    fn speak(&self) -> u64;
}
struct Dog(u64, [u64; 3]);
impl Speak for Dog {
    fn speak(&self) -> u64 {
        self.0 + self.1[2]
    }
}

fn vtable_and_nullary() {
    let d = Dog(1, [2, 3, 4]);
    let r: &dyn Speak = &d;
    let meta = std::ptr::metadata(r);
    println!("vtable = {} {} {}", meta.size_of(), meta.align_of(), r.speak());

    enum Three { _A, _B, _C }
    println!("nullary = {} {} {} {} {} {}",
        std::mem::size_of::<Dog>(), std::mem::align_of::<Dog>(),
        std::mem::variant_count::<Three>(), std::mem::variant_count::<Option<u8>>(),
        std::mem::needs_drop::<String>(), std::mem::needs_drop::<u64>());
}

fn main() {
    float_math();
    atomic_minmax();
    volatile_mem();
    nt_and_mask();
    vtable_and_nullary();
}
