//! The linker arguments this platform's toolchain needs to produce a mirvm image.
//!
//! A mirvm image is a shared object built from one generated `.s`, with no C runtime to start: it
//! borrows the host's dynamic loader for the symbols it does not define and is expected to keep
//! those undefined until load time. Which flags say that is the toolchain's and the object
//! format's business rather than the CPU's, which is why they are here and not in `src/lower/`.

/// Arguments every image link carries, before the output path.
pub const COMMON: &[&str] = &["-shared", "-fPIC"];

/// Extra arguments for the asm-stub image.
///
/// Nothing may be linked into it, so the C runtime is refused outright: an undefined symbol in a
/// stub image would be a bug in the stub factory rather than a deliberate borrow.
pub const ASM_STUB: &[&str] = &["-nostdlib"];

/// Extra arguments for the global-asm image.
///
/// It borrows host symbols on purpose, and `-Bsymbolic` keeps a definition this image carries
/// binding to itself rather than to a same-named host library.
pub const GLOBAL_ASM: &[&str] = &["-nostartfiles", "-Wl,-Bsymbolic"];
