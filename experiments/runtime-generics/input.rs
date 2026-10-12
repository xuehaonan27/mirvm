#![feature(core_intrinsics)]
#![allow(internal_features)]

#[inline(never)]
pub unsafe fn copy_elements<T: Copy, const N: usize>(
    source: *const T,
    target: *mut T,
    _length: usize,
) -> usize {
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

#[derive(Clone, Copy, Default)]
#[repr(align(64))]
struct AlignedZst;

fn check<T: Copy + Default, const N: usize>() {
    let input = [T::default(); N];
    let mut output = [T::default(); N];
    let length = unsafe { copy_elements::<T, N>(input.as_ptr(), output.as_mut_ptr(), 0) };
    assert_eq!(length, N);
    println!("{} {} {}", size_of::<T>(), align_of::<T>(), length);
    std::hint::black_box(output);
}

fn main() {
    check::<u8, 0>();
    check::<u8, 17>();
    check::<u64, 31>();
    check::<[u64; 8], 7>();
    check::<AlignedZst, 9>();
}
