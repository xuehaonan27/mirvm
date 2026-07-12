#!/usr/bin/env mirvm
---
---
#![feature(core_intrinsics)]
#![allow(internal_features)]

use std::ptr::{read_volatile, write_volatile};

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Pair {
    lo: u64,
    hi: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Padded {
    tag: u8,
    value: u32,
}

unsafe fn read_unaligned_volatile<T: Copy>(src: *const T) -> T {
    unsafe { core::intrinsics::unaligned_volatile_load(src) }
}

unsafe fn write_unaligned_volatile<T>(dst: *mut T, value: T) {
    unsafe { core::intrinsics::unaligned_volatile_store(dst, value) }
}

fn main() {
    let mut aligned = 0u64;
    let mut pair = Pair { lo: 0, hi: 0 };
    let mut bytes = [0u8; 8];
    let mut low_align_16 = [0u8; 24];
    let mut padded = Padded { tag: 0, value: 0 };

    let aligned_value = 0x0123_4567_89ab_cdefu64;
    let pair_value = Pair {
        lo: 0x1020_3040_5060_7080,
        hi: 0x90a0_b0c0_d0e0_f000,
    };
    let unaligned_value = 0x89ab_cdefu32;
    let low_align_value = [0xa5u8; 16];
    let padded_value = Padded {
        tag: 0x5a,
        value: 0x1357_9bdf,
    };
    let (aligned_got, pair_got, unaligned_got) = unsafe {
        write_volatile(&mut aligned, aligned_value);
        let aligned_got = read_volatile(&aligned);
        write_volatile(&mut pair, pair_value);
        let pair_got = read_volatile(&pair);

        let unaligned = bytes.as_mut_ptr().add(1).cast::<u32>();
        write_unaligned_volatile(unaligned, unaligned_value);
        let unaligned_got = read_unaligned_volatile(unaligned);

        // `[u8; 16]` 的 guest 对齐只有 1；aligned volatile 不得暗中
        // 把它强化为对齐 8 的宿主整数对。
        let base = low_align_16.as_mut_ptr() as usize;
        let offset = (9 - base % 8) % 8;
        let low_align = low_align_16.as_mut_ptr().add(offset).cast::<[u8; 16]>();
        assert_eq!((low_align as usize) % 8, 1);
        write_volatile(low_align, low_align_value);
        assert_eq!(read_volatile(low_align), low_align_value);

        // `Padded` 的 3 个 padding 字节可以未初始化；VM 只能作 opaque
        // 位型搬运，不能将整个表示解释为 u64。
        write_volatile(&mut padded, padded_value);
        assert_eq!(read_volatile(&padded), padded_value);
        (aligned_got, pair_got, unaligned_got)
    };

    assert_eq!(aligned_got, aligned_value);
    assert_eq!(pair_got, pair_value);
    assert_eq!(unaligned_got, unaligned_value);
    assert_eq!(&bytes[1..5], &unaligned_value.to_ne_bytes());
    println!(
        "volatile aligned={aligned_got:#018x} unaligned={unaligned_got:#010x} pair={:#018x}:{:#018x}",
        pair_got.hi, pair_got.lo
    );
}
