use std::sync::atomic::{AtomicU32, Ordering};

static PANIC_LINE: AtomicU32 = AtomicU32::new(0);
static DROPS: AtomicU32 = AtomicU32::new(0);

#[inline(always)]
#[track_caller]
fn tracked() -> u32 {
    std::panic::Location::caller().line()
}

#[inline(always)]
#[track_caller]
fn forward() -> u32 {
    tracked()
}

#[inline(always)]
fn stop_tracking() -> (u32, u32) {
    (line!(), forward())
}

#[inline(never)]
#[track_caller]
fn tracked_outer() -> (u32, u32) {
    stop_tracking()
}

#[inline(never)]
fn untracked_outer() -> (u32, u32) {
    (line!(), forward())
}

trait Caller {
    #[track_caller]
    fn location(&self) -> u32;
}

impl Caller for () {
    #[track_caller]
    fn location(&self) -> u32 {
        tracked()
    }
}

#[inline(always)]
#[track_caller]
fn virtual_forward(caller: &dyn Caller) -> u32 {
    caller.location()
}

#[inline(always)]
#[track_caller]
fn checked_index<T>(slice: &[T], index: usize) -> &T {
    &slice[index]
}

// A shared generic template must retain the source scope of its inlined assertion.
#[inline(never)]
fn generic_index<T>(slice: &[T], index: usize) -> &T {
    checked_index(slice, index)
}

struct DropProbe;
impl Drop for DropProbe {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline(always)]
#[track_caller]
fn unwind_from_inline() {
    let _drop = DropProbe;
    let index = std::hint::black_box(2);
    checked_index(&[1_u8], index);
}

fn main() {
    let expected = line!() + 1;
    assert_eq!(forward(), expected);
    let (expected, actual) = tracked_outer();
    assert_eq!(actual, expected);
    let (expected, actual) = untracked_outer();
    assert_eq!(actual, expected);
    let caller: &dyn Caller = std::hint::black_box(&());
    let expected = line!() + 1;
    assert_eq!(virtual_forward(caller), expected);

    std::panic::set_hook(Box::new(|info| {
        PANIC_LINE.store(info.location().unwrap().line(), Ordering::Relaxed);
    }));
    let expected = line!() + 1;
    assert!(std::panic::catch_unwind(|| unwind_from_inline()).is_err());
    assert_eq!(PANIC_LINE.load(Ordering::Relaxed), expected);
    assert_eq!(DROPS.load(Ordering::Relaxed), 1);
    assert!(std::panic::catch_unwind(|| generic_index(&[1_u8], 2)).is_err());
    let first = PANIC_LINE.load(Ordering::Relaxed);
    assert!(std::panic::catch_unwind(|| generic_index(&[1_u64], 2)).is_err());
    assert_eq!(PANIC_LINE.load(Ordering::Relaxed), first);
    assert_eq!(
        first,
        include_str!("mir_inlining.rs")
            .lines()
            .position(|line| line == "    checked_index(slice, index)")
            .unwrap() as u32
            + 1
    );
    println!("inlined locations, generic assertions and unwind agree");
}
