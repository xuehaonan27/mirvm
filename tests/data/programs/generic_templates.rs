#![feature(core_intrinsics)]
#![allow(internal_features)]

use std::sync::atomic::{AtomicUsize, Ordering};

#[inline(never)]
unsafe fn copy_elements<T: Copy, const N: usize>(source: *const T, target: *mut T) -> usize {
    let mut index = 0;
    while index < N {
        unsafe {
            let from = core::intrinsics::arith_offset(source, index as isize);
            let to = core::intrinsics::arith_offset(target, index as isize) as *mut T;
            core::intrinsics::copy_nonoverlapping(from, to, 1);
        }
        index = core::intrinsics::wrapping_add(index, 1);
    }
    index
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(align(64))]
struct AlignedZst;

fn check<T: Copy + PartialEq + std::fmt::Debug, const N: usize>(value: T, initial: T) {
    let input = [value; N];
    let mut output = [initial; N];
    assert_eq!(unsafe { copy_elements::<T, N>(input.as_ptr(), output.as_mut_ptr()) }, N);
    assert_eq!(input, output);
    println!("copy {} {} {N}", size_of::<T>(), align_of::<T>());
}

#[inline(never)]
fn signed_const<T, const K: i16>(_: *const T, value: i16) -> i64 {
    if value < K { K as i64 } else { value as i64 }
}

#[inline(never)]
unsafe fn backwards<T>(last: *const T, count: isize) -> *const T {
    unsafe { core::intrinsics::arith_offset(last, count) }
}

static DROPS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct Tracked(u64);
impl Drop for Tracked {
    fn drop(&mut self) { DROPS.fetch_add(1, Ordering::SeqCst); }
}

#[inline(never)]
fn clone_and_unwind<T: Clone>(value: T) {
    let _copy = value.clone();
    panic!("guest cleanup");
}

fn main() {
    check::<u8, 0>(13, 0);
    check::<u8, 17>(213, 0);
    check::<u64, 31>(0xfedcba9876543210, 0);
    check::<[u64; 8], 7>([57; 8], [0; 8]);
    check::<AlignedZst, 9>(AlignedZst, AlignedZst);
    assert_eq!(signed_const::<u8, -7>(std::ptr::null(), -19), -7);
    assert_eq!(signed_const::<u64, 300>(std::ptr::null(), 700), 700);
    let data = [3u64, 5, 9];
    let last = unsafe { data.as_ptr().add(2) };
    assert_eq!(unsafe { backwards(last, -2) }, data.as_ptr());
    std::panic::set_hook(Box::new(|_| {}));
    let value = Tracked(17);
    assert_eq!(value.0, 17);
    assert!(std::panic::catch_unwind(|| clone_and_unwind(value)).is_err());
    assert_eq!(DROPS.load(Ordering::SeqCst), 2);
    println!("scalars, offsets and guest cleanup agree");
}
