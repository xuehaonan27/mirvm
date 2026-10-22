//! AArch64 instruction fields and out-of-range call veneers.

use super::Reloc;

pub(super) unsafe fn apply(kind: Reloc, at: *mut u8, value: u64, veneer: *mut u8) -> bool {
    let field = at.cast::<u32>();
    let instruction = unsafe { field.read_unaligned() };
    let patched = match kind {
        Reloc::Arm64Call => {
            let immediate = |target: u64| {
                let delta = target as i128 - at as usize as i128;
                (delta % 4 == 0 && (-(1 << 27)..(1 << 27)).contains(&delta))
                    .then_some((delta >> 2) as u32 & 0x03ff_ffff)
            };
            if !value.is_multiple_of(4) {
                return false;
            }
            let imm = match immediate(value) {
                Some(imm) => imm,
                None => {
                    let Some(imm) = immediate(veneer as u64) else {
                        return false;
                    };
                    unsafe {
                        veneer.cast::<u32>().write_unaligned(0x5800_0050); // ldr x16, +8
                        veneer.add(4).cast::<u32>().write_unaligned(0xd61f_0200); // br x16
                        veneer.add(8).cast::<u64>().write_unaligned(value);
                    }
                    imm
                }
            };
            instruction & !0x03ff_ffff | imm
        }
        Reloc::Arm64Page21 => {
            let pages = ((value & !0xfff) as i128 - (at as usize & !0xfff) as i128) >> 12;
            if !(-(1 << 20)..(1 << 20)).contains(&pages) {
                return false;
            }
            let immediate = pages as u32;
            instruction & !0x60ff_ffe0 | (immediate & 3) << 29 | (immediate & 0x1f_fffc) << 3
        }
        Reloc::Arm64PageOffset12 => instruction & !0x003f_fc00 | (value as u32 & 0xfff) << 10,
        _ => return false,
    };
    unsafe { field.write_unaligned(patched) };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_and_pages_check_ranges_and_replace_immediates() {
        let mut words = [0u32; 8];
        words[0] = 0x97ff_ffff;
        let at = words.as_mut_ptr().cast::<u8>();
        let veneer = unsafe { at.add(8) };
        unsafe {
            assert!(apply(Reloc::Arm64Call, at, at as u64 + 4, veneer));
            assert_eq!(words[0], 0x9400_0001);
            let far = at as u64 + (1 << 28);
            assert!(apply(Reloc::Arm64Call, at, far, veneer));
            assert_eq!(words[0], 0x9400_0002);
            assert_eq!(veneer.add(8).cast::<u64>().read_unaligned(), far);
            assert!(!apply(Reloc::Arm64Call, at, at as u64 + 1, veneer));
            assert!(apply(
                Reloc::Arm64Page21,
                at,
                (at as u64 & !0xfff) + (1 << 32) - 4096,
                veneer
            ));
            assert!(!apply(
                Reloc::Arm64Page21,
                at,
                (at as u64 & !0xfff) + (1 << 32),
                veneer
            ));
            assert!(apply(
                Reloc::Arm64Page21,
                at,
                (at as u64 & !0xfff) - (1 << 32),
                veneer
            ));
        }
    }
}
