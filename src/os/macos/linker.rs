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
//! - the runtime-interposition bridge is an image of its own rather than an object of the image it
//!   serves, which is what makes this format's link-time binding route the calls to it; see
//!   [`interpose_args`].

/// Arguments every image link carries, before the output path.
pub const COMMON: &[&str] = &["-shared", "-fPIC"];

/// Extra arguments for the asm-stub image.
pub const ASM_STUB: &[&str] = &["-Wl,-undefined,dynamic_lookup", "-Wl,-no_fixup_chains"];

/// Extra arguments for the global-asm image. The same as [`ASM_STUB`]: the two intentions that
/// differ on the other platform are both spelled by the pair of flags above.
pub const GLOBAL_ASM: &[&str] = &["-Wl,-undefined,dynamic_lookup", "-Wl,-no_fixup_chains"];

/// Nothing, and that is the whole answer: this platform expresses the redirection by *what the
/// bridge is* rather than by a flag.
///
/// A call is bound here to the first library in the link that defines it, and the two-level
/// namespace records that choice in the calling image rather than looking it up at run time. So an
/// image that links against a bridge defining `pthread_create` reaches the bridge, while every
/// other image keeps its own binding to the same-named libSystem symbol. Measured, both orders of
/// the two inputs do this, and it holds for symbols libSystem really defines (`getpid`, `signal`)
/// and not only for a probe's.
///
/// The mechanism this format *does* have for it, a `__DATA,__interpose` table, is not one mirvm can
/// use, which is why the bridge is an image of its own here: dyld ignores that section for an image
/// loaded with `dlopen` at all, and even for one in the launch closure its own trace shows the
/// interposing image keeping its binding to the symbol it interposes. The calls to redirect are in
/// the image the bridge is linked into, so that would redirect nothing.
///
/// Answering with the arguments left out is the one answer that must not be given: the image would
/// name no replacement, so its `pthread_create` would reach libSystem and the engine would lose a
/// thread it never learned about, with no diagnostic anywhere. A caller that gets `None` fails the
/// link instead.
pub fn interpose_args(_calls: &[&str]) -> Option<Vec<String>> {
    Some(Vec::new())
}

/// The name the bridge entry for `call` must be defined under for the image to reach it.
///
/// Nothing here renames anything, so the entry carries the call's own name: it is the *definition*
/// the binding has to find.
pub fn bridge_entry_name(call: &str) -> String {
    call.to_string()
}

/// The arguments a native-archive image's link starts with, and the system libraries it ends with.
///
/// The two intentions the other platform's line states with `-z defs` and `-Bsymbolic` have no
/// counterpart here, and one of them is a real loss rather than a different spelling:
///
/// - "take an undefined reference to be resolved from the host at load time" is
///   `-undefined,dynamic_lookup`, which is the opposite of `-z defs`: nothing here refuses an
///   undefined reference outside the libraries below, so an archive whose closure is incomplete
///   links, and the missing symbol surfaces at `dlopen` instead.
/// - "bind a definition this image carries to itself" needs no flag: this format's two-level
///   namespace already binds a symbol to the library that defines it.
///
/// `-no_fixup_chains` is here for `vm/mcload`, which reads the traditional rebase/bind encoding.
///
/// The suffix is empty, and that is measured rather than assumed: libSystem carries every library
/// the other platform's line names, so an image referencing `log`, `pthread_create` or `dlopen`
/// needs no `-l` of its own.
pub const NATIVE_ARCHIVE_PREFIX: &[&str] = &[
    "-shared",
    "-fPIC",
    "-Wl,-no_fixup_chains",
    "-Wl,-undefined,dynamic_lookup",
];

/// Nothing: every library the other platform's line names is in libSystem here.
pub const NATIVE_ARCHIVE_SUFFIX: &[&str] = &[];

/// How this linker is told to take every member of `archive` rather than only the ones something
/// references.
///
/// The whole archive has to go in: the symbols mirvm resolves out of it are reached by name at run
/// time, so nothing references them at link time. `-force_load` names the archive it applies to, so
/// unlike the other platform's mode there is nothing to turn off afterwards.
pub fn whole_archive(archive: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut argument = std::ffi::OsString::from("-Wl,-force_load,");
    argument.push(archive);
    vec![argument]
}

/// The `cc` arguments that turn the bridge's assembly into the artifact an image is linked with,
/// and what that artifact is called.
///
/// A dylib, and that is the mechanism rather than a packaging choice: an image binds a call to the
/// first library that defines it, so the bridge has to be a library of its own for the image's
/// references to land on it. As an object of the image it would define the call without redirecting
/// anything, because an image's own definition does not displace the binding it recorded.
pub const BRIDGE_ARTIFACT: &[&str] = &["-x", "assembler", "-fPIC", "-dynamiclib"];
pub const BRIDGE_ARTIFACT_EXTENSION: &str = "dylib";

/// `-install_name`, because this format records the library's name *in the image that links
/// against it* and the loader looks the library up by that name.
///
/// Without it the name defaults to the path the library was built at, which for a content-addressed
/// artifact is a staging name that is renamed away before anything can load it: measured, an image
/// built against a bridge produced this way fails at `dlopen` naming the temporary.
pub const BRIDGE_INSTALL_NAME: Option<&str> = Some("-install_name");

/// Whether a bridge entry has to be reachable from outside the object it is built into.
///
/// Here it does, and this is the difference the separate library exists for: the calling image binds
/// `pthread_create` to the definition in *another* image, and a loader only offers the definitions
/// an image exports. Measured the other way round, a private entry leaves the caller bound to
/// libSystem and the bridge receives nothing.
pub const BRIDGE_ENTRY_IS_EXPORTED: bool = true;

/// Whether a bridge slot has to be reachable by name through the loader.
///
/// Here it does: the slots live in the bridge library rather than in the image that uses them, so
/// the image's own symbol table does not have them and the loader's search of the load closure is
/// the only way to reach them.
pub const BRIDGE_SLOT_IS_EXPORTED: bool = true;
