#![feature(core_intrinsics)]
#![allow(internal_features)]

#[inline(never)]
fn compute<T, const TAG: u64>(seed: u64) -> u64 {
    let mut value = seed;
    let mut i = 0;
    while i < 64 {
        value = core::intrinsics::wrapping_mul(value, 6364136223846793005);
        value = core::intrinsics::wrapping_add(value, size_of::<T>() as u64);
        value = core::intrinsics::wrapping_add(value, TAG);
        i = core::intrinsics::wrapping_add(i, 1);
    }
    value
}

unsafe extern "C" {
    fn usleep(micros: u32) -> i32;
}

#[inline(never)]
fn pause() {
    unsafe { usleep(50); }
}

#[inline(never)]
fn slow_wrapper<T>(_: &T) -> usize {
    pause();
    size_of::<T>()
}

fn main() {
    let mode = std::env::args().nth(1).unwrap();
    // Both instances are reachable, but a single run uses only the selected compute instance.
    let first = if mode == "alternate" { compute::<u64, 17>(1) } else { compute::<u8, 3>(1) };
    println!("first={first}");
    let mut value = 0;
    for seed in 0..3000 {
        if mode == "sleep" {
            value += (slow_wrapper(&1u8) + slow_wrapper(&1u64)) as u64;
        } else if mode == "alternate" {
            value ^= compute::<u64, 17>(seed);
        } else {
            value ^= compute::<u8, 3>(seed);
        }
    }
    println!("result={value}");
}
