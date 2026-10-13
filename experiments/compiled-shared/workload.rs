#![feature(core_intrinsics)]
#![allow(internal_features)]

#[inline(never)]
fn kernel<T, const TAG: u64>(seed: u64, count: u64) -> u64 {
    let mut value = seed;
    let mut i = 0;
    while i < count {
        value = core::intrinsics::wrapping_mul(value, 6364136223846793005);
        value = core::intrinsics::wrapping_add(value, size_of::<T>() as u64);
        value = core::intrinsics::wrapping_add(value, TAG);
        i = core::intrinsics::wrapping_add(i, 1);
    }
    value
}

fn batch(seed: u64, count: u64, used: usize) -> u64 {
    let mut result = 0;
    macro_rules! instances {
        ($($n:literal),*) => { $(
            if $n < used {
                result ^= kernel::<[u8; $n], $n>(seed, count);
            }
        )* };
    }
    instances!(
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47,
        48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63
    );
    result
}

unsafe extern "C" {
    fn usleep(micros: u32) -> i32;
}

#[inline(never)]
fn native_pause() {
    unsafe {
        usleep(50);
    }
}

#[inline(never)]
fn wrapper<T>(_: &T) -> usize {
    native_pause();
    size_of::<T>()
}

fn main() {
    let case = std::env::args().nth(1).unwrap();
    let (used, rounds, count) = match case.as_str() {
        "sparse" => (64, 2, 8),
        "many" => (64, 512, 32),
        "hot" => (2, 5000, 64),
        "tiny" => (64, 512, 0),
        "native" => (0, 200, 0),
        _ => panic!("unknown workload"),
    };
    // This line follows a completed batch, not a startup marker.
    println!("first={}", batch(17, count, used));
    let mut result = 0;
    for seed in 0..rounds {
        if case == "native" {
            result += (wrapper(&1u8) + wrapper(&1u64)) as u64;
        } else {
            result ^= batch(seed, count, used);
        }
    }
    println!("result={result}");
}
