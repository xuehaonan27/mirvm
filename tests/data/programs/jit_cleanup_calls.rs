use std::cell::Cell;
use std::hint::black_box;
use std::panic::{AssertUnwindSafe, catch_unwind};

struct Guard<'a>(&'a Cell<u32>);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[inline(never)]
fn unit(fail: bool) {
    assert!(!fail, "unit");
}

#[inline(never)]
fn scalar(fail: bool) -> u64 {
    assert!(!fail, "scalar");
    42
}

#[inline(never)]
fn pair(fail: bool, x: u128) -> u128 {
    assert!(!fail, "pair");
    x + 7
}

#[inline(never)]
fn aggregate(fail: bool, x: [u64; 10]) -> [u64; 10] {
    assert!(!fail, "aggregate");
    std::array::from_fn(|i| x[i] + i as u64)
}

#[inline(never)]
fn stack_args(
    a: u64,
    b: u64,
    c: u64,
    d: u64,
    e: u64,
    f: u64,
    g: u64,
    h: u64,
    i: u64,
    j: u64,
) -> u64 {
    assert!(a != 0, "stack");
    a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h + 9 * i + 10 * j
}

#[inline(never)]
fn guarded(which: u32, drops: &Cell<u32>) -> u64 {
    let _guard = Guard(drops);
    unit(which == 1);
    let s = scalar(which == 2);
    let p = pair(which == 3, (1u128 << 100) + 9);
    let a = aggregate(which == 4, [10; 10]);
    let k = stack_args(if which == 5 { 0 } else { 1 }, 2, 3, 4, 5, 6, 7, 8, 9, 10);
    assert_eq!(s, 42);
    assert_eq!(p, (1u128 << 100) + 16);
    assert_eq!(a, [10, 11, 12, 13, 14, 15, 16, 17, 18, 19]);
    assert_eq!(k, 385);
    s + p as u64 + a.iter().sum::<u64>() + k
}

fn main() {
    std::panic::set_hook(Box::new(|_| {}));
    let drops = Cell::new(0);
    for which in 0..=5 {
        let result = catch_unwind(AssertUnwindSafe(|| guarded(black_box(which), &drops)));
        assert_eq!(drops.get(), which + 1);
        if which == 0 {
            assert_eq!(result.unwrap(), 588);
        } else {
            assert!(result.is_err());
        }
        println!("case={which} drops={}", drops.get());
    }
}
