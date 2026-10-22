//! x86-64 has no additional instruction-field relocations in the saved-code vocabulary.

use super::Reloc;

pub(super) unsafe fn apply(_: Reloc, _: *mut u8, _: u64, _: *mut u8) -> bool {
    false
}
