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

/// How this linker is told to send each call in `calls` to mirvm's replacement for it, or `None`
/// when the platform has no way to say it at all.
///
/// `calls` is `crate::vm::interpose::INTERPOSED_CALLS`, whose bridge entries the linker reaches by
/// the `__wrap_` prefix GNU ld's `--wrap=<name>` derives. The list has to arrive whole: a link
/// missing one entry hands that call straight to the host, and the engine loses a thread or a
/// signal it was supposed to own, with no diagnostic anywhere.
///
/// This platform can always express it, which is why the answer here is never `None`; the other
/// side of the ladder is where the same question has a negative answer.
pub fn interpose_args(calls: &[&str]) -> Option<Vec<String>> {
    Some(
        calls
            .iter()
            .map(|name| format!("-Wl,--wrap={name}"))
            .collect(),
    )
}
