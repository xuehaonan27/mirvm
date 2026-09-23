//! The linker arguments this platform's toolchain needs to produce a mirvm image.
//!
//! See the Linux file for what a mirvm image is. This platform's `ld` rejects the GNU spellings the
//! other one uses — `-Bsymbolic` is refused outright and a dylib must link against libSystem — so
//! the same intentions are spelled differently, and measured rather than assumed:
//!
//! - "borrow undefined symbols from the host at load time" is `-Wl,-undefined,dynamic_lookup`
//!   rather than `-nostartfiles`, and it serves both images.
//! - "bind a definition this image carries to itself" needs no flag: this format's two-level
//!   namespace already binds a symbol to the library that defines it.
//! - `-Wl,-no_fixup_chains` keeps relocations in the traditional rebase/bind encoding that
//!   `vm/mcload` can read, instead of the chained fixups this linker emits by default.

/// Arguments every image link carries, before the output path.
pub const COMMON: &[&str] = &["-shared", "-fPIC"];

/// Extra arguments for the asm-stub image.
pub const ASM_STUB: &[&str] = &["-Wl,-undefined,dynamic_lookup", "-Wl,-no_fixup_chains"];

/// Extra arguments for the global-asm image. The same as [`ASM_STUB`]: the two intentions that
/// differ on the other platform are both spelled by the pair of flags above.
pub const GLOBAL_ASM: &[&str] = &["-Wl,-undefined,dynamic_lookup", "-Wl,-no_fixup_chains"];

/// `None`: this format has no flag that redirects the calls `calls` names.
///
/// GNU ld's `--wrap` works on symbol references as the linker resolves them, and Mach-O's two-level
/// namespace has no counterpart. A call is redirected here by a dyld interposing table — an
/// `__DATA,__interpose` section pairing each replacement with the symbol it replaces, which dyld
/// applies to the image as it loads it — and this port does not emit one.
///
/// Answering with the arguments left out is the one answer that must not be given: the image would
/// name no replacement, so its `pthread_create` would reach libSystem and the engine would lose a
/// thread it never learned about, with no diagnostic anywhere. A caller that gets `None` fails the
/// link instead.
pub fn interpose_args(_calls: &[&str]) -> Option<Vec<String>> {
    None
}
