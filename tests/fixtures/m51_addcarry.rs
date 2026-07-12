use std::arch::x86_64::{_addcarry_u64, _subborrow_u64};

fn add(carry: u8, a: u64, b: u64) -> (u8, u64) {
    let mut result = 0;
    let carry = _addcarry_u64(carry, a, b, &mut result);
    (carry, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn add_checksum() -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for (carry, a, b) in [
        (0, u64::MAX, 1),
        (1, u64::MAX, 1),
        (1, u64::MAX, 0),
        (0, 3, 4),
        (1, 3, 4),
    ] {
        let (carry, result) = add(carry, a, b);
        hash = (hash ^ u64::from(carry)).wrapping_mul(0x0000_0100_0000_01b3);
        hash = (hash ^ result).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn sub(borrow: u8, a: u64, b: u64) -> (u8, u64) {
    let mut result = 0;
    let borrow = _subborrow_u64(borrow, a, b, &mut result);
    (borrow, result)
}

#[unsafe(no_mangle)]
pub extern "C" fn sub_checksum() -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for (borrow, a, b) in [(0, 0, 1), (1, 0, 1), (1, 0, 0), (0, 7, 3), (1, 7, 3)] {
        let (borrow, result) = sub(borrow, a, b);
        hash = (hash ^ u64::from(borrow)).wrapping_mul(0x0000_0100_0000_01b3);
        hash = (hash ^ result).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn main() {
    println!("{}", add_checksum());
    println!("{}", sub_checksum());
}
