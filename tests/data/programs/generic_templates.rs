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
    assert_eq!(
        unsafe { copy_elements::<T, N>(input.as_ptr(), output.as_mut_ptr()) },
        N
    );
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
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

#[inline(never)]
fn clone_and_unwind<T: Clone>(value: T) {
    let _copy = value.clone();
    panic!("guest cleanup");
}

#[inline(never)]
fn owned_identity<T>(_: u8, value: T, _: u16) -> T {
    value
}

#[inline(never)]
fn owned_forward<T>(value: T) -> T {
    owned_identity(7, value, 513)
}

#[inline(never)]
fn owned_pick<T>(first: T, choose_first: bool, second: T) -> T {
    if choose_first { first } else { second }
}

#[inline(never)]
fn owned_select<T>(first: T, second: T, choose_first: bool) -> T {
    owned_pick(first, choose_first, second)
}

#[inline(never)]
fn owned_clone<T: Clone>(value: T) -> T {
    let copy = value.clone();
    drop(value);
    copy
}

#[inline(never)]
fn owned_observe<T: Clone + Action>(value: T) -> u64 {
    let copy = value.clone();
    copy.act()
}

#[derive(Clone, Debug, PartialEq)]
#[repr(align(128))]
struct AlignedValue([u8; 73]);

struct FieldPair<T> {
    tag: u8,
    value: T,
    tail: u16,
}

#[repr(C)]
struct CFieldPair<T> {
    tag: u8,
    value: T,
    tail: u16,
}

#[inline(never)]
fn construct_fields<T>(value: T) -> FieldPair<T> {
    FieldPair {
        tag: 3,
        value,
        tail: 517,
    }
}

#[inline(never)]
fn construct_c_fields<T>(value: T) -> CFieldPair<T> {
    CFieldPair {
        tag: 5,
        value,
        tail: 1031,
    }
}

#[inline(never)]
fn construct_tuple<T>(value: T) -> (u8, T, u16) {
    (7, value, 2053)
}

#[inline(never)]
fn generic_field<T>(value: &FieldPair<T>) -> &T {
    &value.value
}

#[inline(never)]
fn generic_field_mut<T>(value: &mut FieldPair<T>) -> &mut T {
    &mut value.value
}

#[inline(never)]
fn generic_tail<T>(value: &FieldPair<T>) -> u16 {
    value.tail
}

#[inline(never)]
fn generic_c_field<T>(value: &CFieldPair<T>) -> &T {
    &value.value
}

#[inline(never)]
fn nested_field<T>(value: &(u8, FieldPair<T>)) -> &T {
    &value.1.value
}

#[inline(never)]
fn into_field<T>(value: FieldPair<T>) -> T {
    value.value
}

fn check_fields<T: Clone + PartialEq + std::fmt::Debug>(value: T) {
    let mut pair = construct_fields(value.clone());
    assert_eq!(pair.tag, 3);
    assert_eq!(generic_tail(&pair), 517);
    assert!(std::ptr::eq(generic_field(&pair), &pair.value));
    assert_eq!(generic_field(&pair), &value);
    *generic_field_mut(&mut pair) = value.clone();
    let nested = (11u8, pair);
    assert!(std::ptr::eq(nested_field(&nested), &nested.1.value));
    assert_eq!(into_field(nested.1), value);
    let c = construct_c_fields(value.clone());
    assert_eq!((c.tag, c.tail), (5, 1031));
    assert!(std::ptr::eq(generic_c_field(&c), &c.value));
    assert_eq!(construct_tuple(value.clone()), (7, value, 2053));
}

fn check_owned<T: Clone + PartialEq + std::fmt::Debug>(value: T) {
    check_fields(value.clone());
    let cloned = owned_clone(value.clone());
    assert_eq!(cloned, value);
    assert_eq!(owned_forward(cloned), value);
    assert_eq!(owned_select(value.clone(), value.clone(), true), value);
    assert_eq!(owned_select(value.clone(), value.clone(), false), value);
    println!("owned {} {}", size_of::<T>(), align_of::<T>());
}

static DROP_ORDER: AtomicUsize = AtomicUsize::new(0);

struct Life {
    id: u64,
    stage: u64,
}

impl Clone for Life {
    fn clone(&self) -> Self {
        if self.stage == 1 {
            panic!("clone failed");
        }
        Self { id: self.id + 1, stage: self.stage }
    }
}
impl Action for Life {
    fn act(&self) -> u64 {
        if self.stage == 2 {
            panic!("observe failed");
        }
        self.id
    }
}
impl Drop for Life {
    fn drop(&mut self) {
        DROP_ORDER.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |order| {
            Some(order * 10 + self.id as usize)
        }).unwrap();
        if self.stage == 3 && self.id == 1 {
            panic!("original drop failed");
        }
    }
}

#[derive(Clone)]
#[repr(align(128))]
struct DropZst;
impl Action for DropZst {
    fn act(&self) -> u64 {
        assert_eq!(self as *const Self as usize % align_of::<Self>(), 0);
        97
    }
}
impl Drop for DropZst {
    fn drop(&mut self) {
        assert_eq!(self as *const Self as usize % align_of::<Self>(), 0);
        DROP_ORDER.fetch_add(1, Ordering::SeqCst);
    }
}

fn check_owned_cleanup() {
    DROP_ORDER.store(0, Ordering::SeqCst);
    assert_eq!(owned_observe(Life { id: 1, stage: 0 }), 2);
    assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 21);
    for (stage, expected) in [(1, 1), (2, 21)] {
        DROP_ORDER.store(0, Ordering::SeqCst);
        assert!(std::panic::catch_unwind(|| owned_observe(Life { id: 1, stage })).is_err());
        assert_eq!(DROP_ORDER.load(Ordering::SeqCst), expected);
    }
    DROP_ORDER.store(0, Ordering::SeqCst);
    assert!(std::panic::catch_unwind(|| owned_clone(Life { id: 1, stage: 3 })).is_err());
    assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 12);
    DROP_ORDER.store(0, Ordering::SeqCst);
    assert_eq!(owned_observe(DropZst), 97);
    assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 2);
    DROP_ORDER.store(0, Ordering::SeqCst);
    let selected = owned_select(Life { id: 1, stage: 0 }, Life { id: 2, stage: 0 }, true);
    assert_eq!(selected.id, 1);
    assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 2);
    drop(selected);
    assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 21);
    DROP_ORDER.store(0, Ordering::SeqCst);
    let selected = owned_select(Life { id: 1, stage: 0 }, Life { id: 2, stage: 0 }, false);
    assert_eq!(selected.id, 2);
    assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 1);
    drop(selected);
    assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 12);
    println!("owned clone, return and unwind drop order agree");
}

#[inline(never)]
fn slice_len<T>(values: &[T]) -> usize {
    values.len()
}

#[inline(never)]
fn slice_identity<T>(values: &[T]) -> &[T] {
    values
}

#[inline(never)]
fn slice_data<T>(values: &[T]) -> *const T {
    values.as_ptr()
}

#[inline(never)]
fn index_ref<T>(values: &[T], index: usize) -> &T {
    &values[index]
}

#[inline(never)]
fn index_mut<T>(values: &mut [T], index: usize) -> &mut T {
    &mut values[index]
}

#[inline(never)]
fn array_ref<T, const N: usize>(values: &[T; N], index: usize) -> &T {
    &values[index]
}

#[inline(never)]
fn nested_ref<'a, T>(values: &[&'a [T]], outer: usize, inner: usize) -> &'a T {
    &values[outer][inner]
}

#[inline(never)]
fn matrix_ref<T, const N: usize>(values: &[[T; N]], outer: usize, inner: usize) -> &T {
    &values[outer][inner]
}

#[inline(never)]
fn first_or<T>(values: &[T], fallback: &T) -> *const T {
    match values {
        [first, ..] => first,
        [] => fallback,
    }
}

#[inline(never)]
fn last_or<T>(values: &[T], fallback: &T) -> *const T {
    match values {
        [.., last] => last,
        [] => fallback,
    }
}

#[inline(never)]
fn fixed_array_last<T>(values: &[T; 3]) -> &T {
    let [_, _, last] = values;
    last
}

#[inline(never)]
fn middle_slice<T>(values: &[T]) -> &[T] {
    match values {
        [_, middle @ .., _] => middle,
        _ => values,
    }
}

#[inline(never)]
fn tail_mut<T>(values: &mut [T]) -> &mut [T] {
    match values {
        [_, tail @ ..] => tail,
        _ => values,
    }
}

fn check_index<T: Copy + PartialEq + std::fmt::Debug + std::panic::RefUnwindSafe>(
    initial: T,
    replacement: T,
) {
    let mut values = [initial; 3];
    assert!(std::ptr::eq(index_ref(&values, 0), &values[0]));
    assert!(std::ptr::eq(array_ref(&values, 2), &values[2]));
    assert!(std::ptr::eq(nested_ref(&[&[], &values], 1, 2), &values[2]));
    *index_mut(&mut values, 2) = replacement;
    assert_eq!(values, [initial, initial, replacement]);
    for (slice, index) in [
        (&values[..], 3),
        (&values[..], usize::MAX),
        (&values[..0], 0),
    ] {
        let drops = DROPS.load(Ordering::SeqCst);
        let panic = std::panic::catch_unwind(|| {
            let _guard = Tracked(0);
            index_ref(slice, index);
        })
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<String>().unwrap(),
            &format!(
                "index out of bounds: the len is {} but the index is {index}",
                slice.len()
            )
        );
        assert_eq!(DROPS.load(Ordering::SeqCst), drops + 1);
    }
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            index_mut(&mut values, 3);
        }))
        .is_err()
    );
    assert!(std::panic::catch_unwind(|| array_ref(&values, 3)).is_err());
    assert!(std::panic::catch_unwind(|| nested_ref(&[&values], 1, 0)).is_err());
    assert!(std::panic::catch_unwind(|| nested_ref(&[&values], 0, 3)).is_err());
    let matrix = [[initial; 3], [replacement; 3]];
    assert!(std::ptr::eq(matrix_ref(&matrix, 1, 2), &matrix[1][2]));
    assert!(std::panic::catch_unwind(|| matrix_ref(&matrix, 2, 0)).is_err());
    assert!(std::panic::catch_unwind(|| matrix_ref(&matrix, 1, 3)).is_err());
    assert!(std::ptr::eq(fixed_array_last(&values), &values[2]));
    for len in 0..=values.len() {
        let slice = &values[..len];
        let first = slice.first().unwrap_or(&initial);
        let last = slice.last().unwrap_or(&initial);
        assert!(std::ptr::eq(first_or(slice, &initial), first));
        assert!(std::ptr::eq(last_or(slice, &initial), last));
        let middle = middle_slice(slice);
        let expected = if len < 2 { slice } else { &slice[1..len - 1] };
        assert_eq!(middle, expected);
        assert!(std::ptr::eq(middle.as_ptr(), expected.as_ptr()));
        let data = unsafe { values.as_ptr().add(usize::from(len != 0)) };
        let tail = tail_mut(&mut values[..len]);
        assert_eq!(tail.len(), len.saturating_sub(1));
        assert_eq!(tail.as_ptr(), data);
        if !tail.is_empty() {
            tail[0] = replacement;
            assert_eq!(values[1], replacement);
        }
    }
    println!("indexed {} {}", size_of::<T>(), align_of::<T>());
}

#[inline(never)]
fn copy_slice<T: Copy>(values: &[T], target: *mut T) -> usize {
    unsafe {
        std::ptr::copy_nonoverlapping(values.as_ptr(), target, values.len());
    }
    values.len()
}

#[inline(never)]
fn layout_constants<T>(_: *const T) -> (usize, usize) {
    (size_of::<T>(), align_of::<T>())
}

#[inline(never)]
fn fixed_field<T>(pair: &(*const T, usize)) -> usize {
    pair.1
}

#[inline(never)]
fn fixed_ref<T>(pair: &(u8, *const T)) -> &u8 {
    &pair.0
}

fn check_slice<T: Copy + PartialEq + std::fmt::Debug>(values: &[T], target: &mut [T]) {
    assert_eq!(slice_len(values), values.len());
    let identity = slice_identity(values);
    assert_eq!(identity.len(), values.len());
    assert_eq!(identity.as_ptr(), values.as_ptr());
    assert_eq!(slice_data(values), values.as_ptr());
    assert_eq!(fixed_field(&(values.as_ptr(), values.len())), values.len());
    assert_eq!(*fixed_ref(&(37, values.as_ptr())), 37);
    assert_eq!(copy_slice(values, target.as_mut_ptr()), values.len());
    assert_eq!(values, target);
}

trait Action {
    fn act(&self) -> u64;
}
struct First(u64);
struct Second(u64);
struct Throws;
impl Action for First {
    fn act(&self) -> u64 {
        self.0 + 11
    }
}
impl Action for Second {
    fn act(&self) -> u64 {
        self.0 + 29
    }
}
impl Action for Throws {
    fn act(&self) -> u64 {
        let _guard = Tracked(9);
        panic!("witness cleanup");
    }
}

#[inline(never)]
fn witness<T: Action>(value: &T) -> u64 {
    value.act()
}

#[inline(never)]
fn clone_reference<'a, T>(value: &&'a T) -> &'a T {
    Clone::clone(value)
}

struct OtherDrop(u64);
impl Drop for OtherDrop {
    fn drop(&mut self) {
        DROPS.fetch_add(self.0 as usize, Ordering::SeqCst);
    }
}

#[inline(never)]
unsafe fn destroy<T>(value: *mut T) {
    unsafe {
        std::ptr::drop_in_place(value);
    }
}

trait Located {
    #[track_caller]
    fn line(&self) -> u32;
}
impl Located for First {
    #[track_caller]
    fn line(&self) -> u32 {
        std::panic::Location::caller().line()
    }
}

#[inline(never)]
fn caller_location<T: Located>(value: &T) -> u32 {
    value.line()
}

#[inline(never)]
fn indirect<T>(value: &T, call: fn(&T) -> u64) -> u64 {
    call(value)
}

#[inline(never)]
fn recursive<T>(value: T, depth: usize) -> T {
    if depth == 0 { value } else { recursive(value, depth.wrapping_sub(1)) }
}

unsafe extern "C" {
    fn qsort(base: *mut u64, count: usize, size: usize,
             compare: extern "C" fn(*const u64, *const u64) -> i32);
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn _exit(code: i32) -> !;
}

extern "C" fn compare(a: *const u64, b: *const u64) -> i32 {
    let (a, b) = unsafe { (owned_identity(0, *a, 0), recursive(*b, 3)) };
    if a < b { -1 } else if a == b { 0 } else { 1 }
}

fn check_shared_entries() {
    let barrier = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        for i in 0..4u64 {
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                for depth in 0..8 {
                    assert_eq!(recursive(i, depth), i);
                    assert_eq!(owned_identity(0, i, 0), i);
                }
            });
        }
    });
    assert_eq!(recursive(17u8, 13), 17);
    let mut values = [19u64, 3, 17, 11];
    unsafe { qsort(values.as_mut_ptr(), values.len(), size_of::<u64>(), compare); }
    assert_eq!(values, [3, 11, 17, 19]);
}

fn check_fork() {
    let pid = unsafe { fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let value = owned_forward([37u64; 8]);
        unsafe { _exit(if value == [37; 8] { 0 } else { 9 }); }
    }
    let mut status = 0;
    assert_eq!(unsafe { waitpid(pid, &mut status, 0) }, pid);
    assert_eq!(status, 0);
}

fn main() {
    assert_eq!(layout_constants(std::ptr::null::<[u8; 9]>()), (9, 1));
    assert_eq!(layout_constants(std::ptr::null::<u64>()), (8, 8));
    assert_eq!(layout_constants(std::ptr::null::<AlignedZst>()), (0, 64));
    if std::env::args().nth(1).as_deref() == Some("fork") {
        check_fork();
        println!("shared fork agrees");
        return;
    }
    check_shared_entries();
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
    check_owned(213u8);
    check_owned(0xfedcba9876543210u64);
    check_owned((29u8, 0xfedcba9876543210u64));
    check_owned([0xfedcba9876543210u64; 8]);
    check_owned(u128::MAX - 19);
    check_owned(17.25f64);
    check_owned(AlignedZst);
    check_owned(AlignedValue([197; 73]));
    check_owned_cleanup();
    let value = Tracked(17);
    assert_eq!(value.0, 17);
    assert!(std::panic::catch_unwind(|| clone_and_unwind(value)).is_err());
    assert_eq!(DROPS.load(Ordering::SeqCst), 2);
    check_slice(&[1u8, 3, 5], &mut [0u8; 3]);
    check_slice(&[17u64, 31], &mut [0u64; 2]);
    check_slice(&[AlignedZst; 5], &mut [AlignedZst; 5]);
    check_slice::<u64>(&[], &mut []);
    assert_eq!(witness(&First(7)), 18);
    assert_eq!(witness(&Second(7)), 36);
    assert_eq!(indirect(&First(7), First::act), 18);
    assert!(std::panic::catch_unwind(|| witness(&Throws)).is_err());
    assert_eq!(DROPS.load(Ordering::SeqCst), 3);
    let number = 79u64;
    assert!(std::ptr::eq(clone_reference(&&number), &number));
    let mut first = std::mem::ManuallyDrop::new(Tracked(8));
    let mut second = std::mem::ManuallyDrop::new(OtherDrop(10));
    unsafe {
        destroy(&mut *first);
        destroy(&mut *second);
    }
    assert_eq!(DROPS.load(Ordering::SeqCst), 14);
    println!("caller location {}", caller_location(&First(0)));
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().unwrap();
        let message = info.payload().downcast_ref::<String>().unwrap();
        println!("bounds {}:{} {message}", location.line(), location.column());
    }));
    check_index(1u8, 9);
    check_index(13u64, 31);
    check_index([7u64; 8], [17; 8]);
    check_index(AlignedZst, AlignedZst);
    assert!(std::panic::catch_unwind(|| array_ref::<u8, 0>(&[], 0)).is_err());
    assert_eq!(*matrix_ref(&[[3u64, 5], [7, 11]], 1, 1), 11);
    assert!(std::panic::catch_unwind(|| matrix_ref::<u8, 0>(&[[]], 0, 0)).is_err());
    println!("scalars, offsets and guest cleanup agree");
}
