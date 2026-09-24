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
/// `calls` is `crate::vm::interpose::INTERPOSED_CALLS`. The list has to arrive whole: a link
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

/// The name the bridge entry for `call` must be defined under for the linker to route `call` to it.
///
/// `--wrap=<name>` is a rename of the reference: every reference to `<name>` becomes a reference to
/// `__wrap_<name>`, so the entry that receives the call has to carry that spelling.
pub fn bridge_entry_name(call: &str) -> String {
    format!("__wrap_{call}")
}

/// The `cc` arguments that turn the bridge's assembly into the artifact an image is linked with,
/// and what that artifact is called.
///
/// An object file: this linker routes the call by name at link time, so the entry only has to be
/// one of the link's inputs. Nothing is recorded in it by path, so [`BRIDGE_INSTALL_NAME`] is
/// `None`.
pub const BRIDGE_ARTIFACT: &[&str] = &["-x", "assembler", "-fPIC", "-c"];
pub const BRIDGE_ARTIFACT_EXTENSION: &str = "o";
pub const BRIDGE_INSTALL_NAME: Option<&str> = None;

/// Whether a bridge entry has to be reachable from outside the object it is built into.
///
/// Not here: `--wrap` renames the reference within the one link the entry is an input of, so the
/// entry is reached by a name that exists only inside that link.
pub const BRIDGE_ENTRY_IS_EXPORTED: bool = false;

/// Whether a bridge slot has to be reachable by name through the loader.
///
/// Not here, and it must not be: the entries reach their slots PC-relative, which this format only
/// allows for a symbol the link resolves itself. An exported slot is preemptible, so the relocation
/// is refused outright — measured, the link fails with `R_X86_64_PC32 ... can not be used when
/// making a shared object`. The slots stay private and the engine reads them out of the image's own
/// symbol table.
pub const BRIDGE_SLOT_IS_EXPORTED: bool = false;
