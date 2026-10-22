use std::cell::Cell;
use std::hint::black_box;

struct Guard<'a>(&'a Cell<usize>);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[repr(align(64))]
#[derive(Clone, Copy)]
struct Payload([u64; 8]);

#[inline(never)]
fn aggregate<T: Copy + Into<u128>>(
    seed: T,
    n: u64,
    entries: &Cell<usize>,
    drops: &Cell<usize>,
) -> Payload {
    entries.set(entries.get() + 1);
    let _guard = Guard(drops);
    let mut local = Payload([0; 8]);
    let address = &mut local as *mut Payload;
    assert_eq!(address as usize % 64, 0);
    let mut narrow = 3u8;
    let mut wide = seed.into();
    for outer in 0..3 {
        for i in 0..n {
            let i = black_box(i);
            narrow = narrow.wrapping_add(i as u8);
            wide = wide.wrapping_add((i as u128) << 64 | outer);
            unsafe {
                (*address).0[0] = (*address).0[0].wrapping_add(i);
            }
        }
    }
    local.0[1] = narrow as u64;
    local.0[2] = wide as u64;
    local.0[3] = (wide >> 64) as u64;
    assert_eq!(address, &mut local as *mut Payload);
    local
}

#[inline(never)]
fn pair(n: u64) -> (u64, u64) {
    let mut a = 7u64;
    let mut b = 11u64;
    for i in 0..n {
        a = a.wrapping_add(black_box(i));
        b ^= a;
    }
    (a, b)
}

#[inline(never)]
fn panic_after_loop(n: u64, entries: &Cell<usize>, drops: &Cell<usize>) {
    entries.set(entries.get() + 1);
    let _guard = Guard(drops);
    let mut total = 0u64;
    for i in 0..n {
        total = total.wrapping_add(black_box(i));
    }
    assert_eq!(total, n * (n - 1) / 2);
    panic!("OSR cleanup probe");
}

fn main() {
    let entries = Cell::new(0);
    let drops = Cell::new(0);
    let n = black_box(1_000_003u64);
    let result = aggregate(17u64, n, &entries, &drops);
    assert_eq!(result.0[0], 3 * n * (n - 1) / 2);
    assert_eq!(result.0[2], 17 + 3 * n);
    assert_eq!(result.0[3], result.0[0]);
    assert_eq!((entries.get(), drops.get()), (1, 1));
    let p = pair(n);
    assert_eq!(p.0, 7 + n * (n - 1) / 2);
    std::panic::set_hook(Box::new(|_| {}));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| panic_after_loop(
            n, &entries, &drops
        )))
        .is_err()
    );
    assert_eq!((entries.get(), drops.get()), (2, 2));
    println!(
        "osr-ok {:?} {:?} {} {}",
        result.0,
        p,
        entries.get(),
        drops.get()
    );
}
