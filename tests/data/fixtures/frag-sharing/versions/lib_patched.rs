//! The dependency whose lowered bodies the fragment-sharing probe measures: a family of small
//! functions (so many bodies exist), a frozen table of function pointers (so canonical bodies carry
//! entry references), and a feature-gated family (so two feature sets can be compared).

pub fn add(x: u64) -> u64 {
    x.wrapping_add(9)
}

pub fn sub(x: u64) -> u64 {
    x.wrapping_sub(5)
}

pub fn mul(x: u64) -> u64 {
    x.wrapping_mul(7)
}

pub fn xor(x: u64) -> u64 {
    x ^ 0x9e37_79b9
}

/// A frozen table of function pointers: every body that reads it names an entry of the same unit.
pub static OPS: [fn(u64) -> u64; 4] = [add, sub, mul, xor];

macro_rules! family {
    ($($name:ident => $k:expr,)*) => {
        $(
            pub fn $name(x: u64) -> u64 {
                x.wrapping_mul($k).rotate_left(3) ^ $k
            }
        )*

        /// One body that calls every member, so the family stays reachable through the program.
        pub fn family_sum(x: u64) -> u64 {
            0u64 $(.wrapping_add($name(x)))*
        }
    };
}

family! {
    f01 => 0x11,
    f02 => 0x22,
    f03 => 0x33,
    f04 => 0x44,
    f05 => 0x55,
    f06 => 0x66,
    f07 => 0x77,
    f08 => 0x88,
    f09 => 0x99,
    f10 => 0xaa,
    f11 => 0xbb,
    f12 => 0xcc,
}

/// The program's entry into the crate: the loop indexes the table, so the four tabled functions and
/// the whole family are lowered.
pub fn table_sum(n: u64) -> u64 {
    let mut acc = 0u64;
    let mut i = 0u64;
    while i < n {
        acc = acc.wrapping_add(OPS[(i % 4) as usize](i));
        i += 1;
    }
    acc.wrapping_add(family_sum(n))
}

#[cfg(feature = "wide")]
pub fn wide_sum(x: u64) -> u64 {
    wide0(x) ^ wide1(x) ^ wide2(x) ^ wide3(x)
}

#[cfg(not(feature = "wide"))]
pub fn wide_sum(_x: u64) -> u64 {
    0
}

#[cfg(feature = "wide")]
fn wide0(x: u64) -> u64 {
    x.rotate_left(1).wrapping_add(1)
}

#[cfg(feature = "wide")]
fn wide1(x: u64) -> u64 {
    x.rotate_left(2).wrapping_add(2)
}

#[cfg(feature = "wide")]
fn wide2(x: u64) -> u64 {
    x.rotate_left(3).wrapping_add(3)
}

#[cfg(feature = "wide")]
fn wide3(x: u64) -> u64 {
    x.rotate_left(4).wrapping_add(4)
}
