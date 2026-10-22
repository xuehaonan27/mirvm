//! Relocations in saved machine code. The owner supplies checked writable fields and veneers.

#[cfg(target_arch = "aarch64")]
#[path = "aarch64/code.rs"]
mod cpu;
#[cfg(target_arch = "x86_64")]
#[path = "x86_64/code.rs"]
mod cpu;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Reloc {
    Abs32,
    Abs64,
    PcRel32,
    Arm64Call,
    Arm64Page21,
    Arm64PageOffset12,
}

impl Reloc {
    pub(crate) fn width(self) -> usize {
        if self == Self::Abs64 { 8 } else { 4 }
    }

    pub(crate) fn veneer_size(self) -> usize {
        if self == Self::Arm64Call { 16 } else { 0 }
    }
}

/// Patch one field, returning false if the address cannot be encoded on this CPU.
///
/// # Safety
/// `at` must address `kind.width()` writable bytes, and `veneer` must address
/// `kind.veneer_size()` writable bytes. Both belong to unpublished code.
pub(crate) unsafe fn apply(kind: Reloc, at: *mut u8, value: u64, veneer: *mut u8) -> bool {
    unsafe {
        match kind {
            Reloc::Abs32 => {
                let Ok(value) = u32::try_from(value) else {
                    return false;
                };
                at.cast::<u32>().write_unaligned(value);
            }
            Reloc::Abs64 => at.cast::<u64>().write_unaligned(value),
            Reloc::PcRel32 => {
                let Ok(delta) = i32::try_from(value as i128 - at as usize as i128) else {
                    return false;
                };
                at.cast::<i32>().write_unaligned(delta);
            }
            _ => return cpu::apply(kind, at, value, veneer),
        }
    }
    true
}
