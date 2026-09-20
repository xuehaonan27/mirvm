//! Linux/ELF static native archive loader: materialize a constraint-checked PIC `.a` into a dlopen-able `.so`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rustc_hir::attrs::NativeLibKind;
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_middle::ty::TyCtxt;
use rustc_session::search_paths::PathKind;
use rustc_target::spec::{BinaryFormat, Os};

const CACHE_FORMAT_VERSION: &[u8] = b"mirvm-native-archive-v4";
const LINK_PREFIX: &[&str] = &[
    "-shared",
    "-Wl,-z,defs",
    "-Wl,-z,text",
    "-Wl,-Bsymbolic",
    "-Wl,--whole-archive",
];
/// Closure baseline: the system libraries that std contributes to the guest's final link via
/// `#[link]` (glibc: m/dl/pthread/rt/util/gcc_s). They are always present in native semantics, so a
/// C static archive may reference their symbols directly -- libsqlite3's FTS5 references libm `log`
/// and the pthread family, for instance.
/// They become DT_NEEDED entries of the produced .so and are resolved by the host environment at
/// dlopen time; `-z defs` still loudly rejects undefined references outside this set (cross-archive
/// and guest symbols), so the closure requirement is not relaxed.
const LINK_SUFFIX: &[&str] = &[
    "-Wl,--no-whole-archive",
    "-lm",
    "-ldl",
    "-lpthread",
    "-lrt",
    "-lutil",
    "-lgcc_s",
];
pub(crate) const NATIVE_RUNTIME_WRAP_FLAGS: &[&str] = &[
    "-Wl,--wrap=pthread_create",
    "-Wl,--wrap=pthread_key_create",
    "-Wl,--wrap=pthread_setspecific",
    "-Wl,--wrap=pthread_key_delete",
    "-Wl,--wrap=signal",
    "-Wl,--wrap=sigaction",
    "-Wl,--wrap=raise",
];
pub(crate) const NATIVE_RUNTIME_BRIDGE_ASM: &str = r#"
.intel_syntax noprefix
.text
.p2align 4
.globl __wrap_pthread_create
.hidden __wrap_pthread_create
.type __wrap_pthread_create,@function
__wrap_pthread_create:
    mov r8, QWORD PTR [rip + __mirvm_pthread_owner]
    jmp QWORD PTR [rip + __mirvm_pthread_create_target]
.size __wrap_pthread_create,.-__wrap_pthread_create

.p2align 4
.globl __wrap_pthread_key_create
.hidden __wrap_pthread_key_create
.type __wrap_pthread_key_create,@function
__wrap_pthread_key_create:
    mov rdx, QWORD PTR [rip + __mirvm_pthread_owner]
    jmp QWORD PTR [rip + __mirvm_pthread_key_create_target]
.size __wrap_pthread_key_create,.-__wrap_pthread_key_create

.p2align 4
.globl __wrap_pthread_setspecific
.hidden __wrap_pthread_setspecific
.type __wrap_pthread_setspecific,@function
__wrap_pthread_setspecific:
    mov rdx, QWORD PTR [rip + __mirvm_pthread_owner]
    jmp QWORD PTR [rip + __mirvm_pthread_setspecific_target]
.size __wrap_pthread_setspecific,.-__wrap_pthread_setspecific

.p2align 4
.globl __wrap_pthread_key_delete
.hidden __wrap_pthread_key_delete
.type __wrap_pthread_key_delete,@function
__wrap_pthread_key_delete:
    mov rsi, QWORD PTR [rip + __mirvm_pthread_owner]
    jmp QWORD PTR [rip + __mirvm_pthread_key_delete_target]
.size __wrap_pthread_key_delete,.-__wrap_pthread_key_delete

.p2align 4
.globl __wrap_signal
.hidden __wrap_signal
.type __wrap_signal,@function
__wrap_signal:
    mov rdx, QWORD PTR [rip + __mirvm_signal_owner]
    jmp QWORD PTR [rip + __mirvm_signal_target]
.size __wrap_signal,.-__wrap_signal

.p2align 4
.globl __wrap_sigaction
.hidden __wrap_sigaction
.type __wrap_sigaction,@function
__wrap_sigaction:
    mov rcx, QWORD PTR [rip + __mirvm_signal_owner]
    jmp QWORD PTR [rip + __mirvm_sigaction_target]
.size __wrap_sigaction,.-__wrap_sigaction

.p2align 4
.globl __wrap_raise
.hidden __wrap_raise
.type __wrap_raise,@function
__wrap_raise:
    mov rsi, QWORD PTR [rip + __mirvm_signal_owner]
    jmp QWORD PTR [rip + __mirvm_raise_target]
.size __wrap_raise,.-__wrap_raise

.pushsection .data.mirvm_pthread,"aw",@progbits
.p2align 3
.globl __mirvm_pthread_owner
.hidden __mirvm_pthread_owner
.type __mirvm_pthread_owner,@object
.size __mirvm_pthread_owner,8
__mirvm_pthread_owner:
    .quad 0
.globl __mirvm_pthread_create_target
.hidden __mirvm_pthread_create_target
.type __mirvm_pthread_create_target,@object
.size __mirvm_pthread_create_target,8
__mirvm_pthread_create_target:
    .quad 0
.globl __mirvm_pthread_key_create_target
.hidden __mirvm_pthread_key_create_target
.type __mirvm_pthread_key_create_target,@object
.size __mirvm_pthread_key_create_target,8
__mirvm_pthread_key_create_target:
    .quad 0
.globl __mirvm_pthread_setspecific_target
.hidden __mirvm_pthread_setspecific_target
.type __mirvm_pthread_setspecific_target,@object
.size __mirvm_pthread_setspecific_target,8
__mirvm_pthread_setspecific_target:
    .quad 0
.globl __mirvm_pthread_key_delete_target
.hidden __mirvm_pthread_key_delete_target
.type __mirvm_pthread_key_delete_target,@object
.size __mirvm_pthread_key_delete_target,8
__mirvm_pthread_key_delete_target:
    .quad 0
.popsection

.pushsection .data.mirvm_signal,"aw",@progbits
.p2align 3
.globl __mirvm_signal_owner
.hidden __mirvm_signal_owner
.type __mirvm_signal_owner,@object
.size __mirvm_signal_owner,8
__mirvm_signal_owner:
    .quad 0
.globl __mirvm_signal_target
.hidden __mirvm_signal_target
.type __mirvm_signal_target,@object
.size __mirvm_signal_target,8
__mirvm_signal_target:
    .quad 0
.globl __mirvm_sigaction_target
.hidden __mirvm_sigaction_target
.type __mirvm_sigaction_target,@object
.size __mirvm_sigaction_target,8
__mirvm_sigaction_target:
    .quad 0
.globl __mirvm_raise_target
.hidden __mirvm_raise_target
.type __mirvm_raise_target,@object
.size __mirvm_raise_target,8
__mirvm_raise_target:
    .quad 0
.popsection
.section .note.GNU-stack,"",@progbits
"#;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Collect the system dynamic library names the crate graph propagates.
/// A `-sys` crate's `cargo:rustc-link-lib` only writes rlib metadata: the native final link
/// command line carries these `-l` entries, while the metadata driver (the bin command line) does
/// not. When C objects in a static archive reference such a library (libgit2.a's crc32/deflate ->
/// libz-sys's `z`, for instance), the closure link line must include it too. Same collection scope
/// as lower's RTLD_GLOBAL preload, consumed in both places.
/// Static { bundle: None | Some(true) } are true static archives that go entirely into the rlib
/// (handled by the archive path above, skipped here); Framework / LinkArg / wasm are not handled.
pub(crate) fn system_dylibs(tcx: TyCtxt<'_>) -> Vec<Box<str>> {
    let sess = tcx.sess;
    let mut names: Vec<Box<str>> = Vec::new();
    for cnum in std::iter::once(LOCAL_CRATE).chain(tcx.used_crates(()).iter().copied()) {
        if cnum != LOCAL_CRATE && tcx.crate_dep_kind(cnum).macros_only() {
            continue;
        }
        for lib in tcx.native_libraries(cnum) {
            // System dynamic-link kinds = Dylib/RawDylib + Unspecified (bare `-l ssl`, Dylib
            // is the default) + Static { bundle: false } (objects do not go into the rlib,
            // resolved as system libraries at link time—libc's m/dl/pthread/rt/util are this shape)
            let system_dylib = matches!(
                lib.kind,
                NativeLibKind::Dylib { .. }
                    | NativeLibKind::RawDylib { .. }
                    | NativeLibKind::Unspecified
            ) || matches!(
                lib.kind,
                NativeLibKind::Static {
                    bundle: Some(false),
                    ..
                }
            );
            if !system_dylib {
                continue;
            }
            if let Some(cfg) = &lib.cfg
                && !rustc_attr_parsing::eval_config_entry(sess, cfg).as_bool()
            {
                continue;
            }
            let name: Box<str> = lib.name.as_str().into();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    // Same scope as CLI `-l` (search-path form is handled by the caller separately)
    for lib in &sess.opts.libs {
        if matches!(lib.kind, NativeLibKind::Static { .. }) {
            continue;
        }
        let name: Box<str> = lib.name.as_str().into();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[cfg(test)]
pub(crate) fn materialize_in(archive: &Path, cache_dir: &Path) -> Result<PathBuf, String> {
    materialize_for_target_in(
        archive,
        cache_dir,
        crate::options::build::HOST,
        Path::new("cc"),
        &[],
        None,
    )
}

/// Collect the current crate graph's Static native libraries and convert each independent archive
/// into a `.so`.
///
/// Only Linux/ELF is handled. Each archive is linked independently with `-z defs`, so cross-archive
/// dependencies, dependency ordering and non-PIC relocations fail loudly; no generic native link
/// plan is guessed.
/// On conversion failure, fall back to the "symbols in rlib" rescue chain
/// (undefined ∩ crate-graph rlib exported fn => inject hidden P1-entry trampolines and relink;
/// unit tests with `linker = None` take the original error path directly).
pub(crate) fn materialize_static_libraries<'tcx>(
    tcx: TyCtxt<'tcx>,
    linker: &mut crate::lower::linker::Linker<'tcx>,
) -> Result<Vec<Box<str>>, String> {
    let sess = tcx.sess;
    let search_dirs: Vec<_> = sess
        .target_filesearch()
        .cli_search_paths(PathKind::Native)
        .map(|path| path.dir.clone())
        .collect();
    let target = sess.opts.target_triple.tuple();
    let cache = crate::options::get().home.join("native-archives");
    let mut shared_objects = Vec::<PathBuf>::new();
    // Crate-graph system dynamic libraries: when a static archive's C objects reference `-l`
    // library symbols propagated via metadata, the closure link line must include them; same list
    // as lower's RTLD_GLOBAL preload.
    let extra_libs = system_dylibs(tcx);

    for cnum in std::iter::once(LOCAL_CRATE).chain(tcx.used_crates(()).iter().copied()) {
        if cnum != LOCAL_CRATE && tcx.crate_dep_kind(cnum).macros_only() {
            continue;
        }
        let crate_name = tcx.crate_name(cnum);
        for lib in tcx.native_libraries(cnum) {
            let NativeLibKind::Static { export_symbols, .. } = lib.kind else {
                continue;
            };
            if let Some(cfg) = &lib.cfg
                && !rustc_attr_parsing::eval_config_entry(sess, cfg).as_bool()
            {
                continue;
            }
            if target != crate::options::build::HOST
                || sess.target.os != Os::Linux
                || sess.target.binary_format != BinaryFormat::Elf
            {
                return Err(format!(
                    "crate `{crate_name}`'s Static native library `{}` can only be handled by the current host \
                     Linux/ELF archive-loading slice (host: {}, current target: {target})",
                    lib.name,
                    crate::options::build::HOST
                ));
            }
            if export_symbols.is_some() {
                return Err(format!(
                    "crate `{crate_name}`'s Static native library `{}` uses the \
                     `+/-export-symbols` modifier; M5.1 archive loading has not defined its `.so` equivalent semantics",
                    lib.name
                ));
            }

            let verbatim = lib.verbatim.unwrap_or(false);
            let filename = if let Some(filename) = lib.filename {
                filename.as_str().to_owned()
            } else {
                let (prefix, suffix) = sess.staticlib_components(verbatim);
                format!("{prefix}{}{suffix}", lib.name)
            };
            let archive = find_archive(&filename, &search_dirs).ok_or_else(|| {
                let searched = search_dirs
                    .iter()
                    .map(|dir| dir.join(&filename).display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Cannot find crate `{crate_name}`'s Static native library `{}` (file `{filename}`); \
                     rustc native search paths: [{}]",
                    lib.name, searched
                )
            })?;
            let so = materialize_for_target_in(
                &archive,
                &cache,
                target,
                Path::new("cc"),
                &extra_libs,
                Some(linker),
            )?;
            if !shared_objects.contains(&so) {
                shared_objects.push(so);
            }
        }
    }
    reject_symbol_ambiguity(&shared_objects)?;
    Ok(shared_objects
        .into_iter()
        .map(|path| path.display().to_string().into())
        .collect())
}

/// Reject ambiguity at materialization time: archive **.dynsym-visible** exported symbols must not
/// **share names across archives**, because resolution would then depend on load order and we
/// refuse to guess native linker order.
///
/// Duplicate symbols follow native link semantics: **all-weak definitions are allowed**
/// (weak/COMDAT first-wins; load order is isomorphic to crate-graph order and native link
/// order -- the three risc0 `-sys` crates each export the C++ sized-delete `_ZdlPvS_` COMDAT, which
/// belongs to this family); **exactly one strong definition is allowed** (strong wins over weak,
/// the same silent resolution native gives); **two or more strong definitions are rejected**
/// (native would already be a link error, so we also reject loudly).
///
/// A name that also exists in the **RTLD_DEFAULT scope** is not rejected as a collision: the
/// resolution order is ① hidden fallback table → ② archive handle (link order) → ③ global
/// dlsym.
/// Objects linked by the guest (hidden or dynsym-visible) always beat host-process libraries of the
/// same name, which is native link-time binding. psm's `rust_psm_on_stack` versus the embedded copy
/// in the host `librustc_driver` is one example; zstd-sys's ZSTD_* versus libLLVM's embedded
/// library is the same shape. Known residual: **intra-archive** cross-references to colliding
/// symbols still go through the dynamic linker's global order (cannot be mirrored; no such shape is
/// known -- the four psm symbols are only called from the Rust side with no internal
/// cross-references).
///
/// Hidden symbols that do not enter .dynsym (carried by the .symtab fallback table) deliberately
/// skip all collision checks: in resolution order they always precede the global scope, so
/// collisions already resolve to the archive, leaving no ambiguity to reject.
fn reject_symbol_ambiguity(shared_objects: &[PathBuf]) -> Result<(), String> {
    let mut owners = HashMap::<String, (PathBuf, bool)>::new();
    for shared_object in shared_objects {
        let output = Command::new("nm")
            .args(["--dynamic", "--defined-only", "--format=posix"])
            .arg(shared_object)
            .output()
            .map_err(|e| {
                format!(
                    "Cannot inspect exported symbols of archive shared library `{}` (failed to launch nm): {e}",
                    shared_object.display()
                )
            })?;
        if !output.status.success() {
            return Err(format!(
                "Cannot inspect exported symbols of archive shared library `{}` (nm failed):\n{}{}",
                shared_object.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let symbols = String::from_utf8(output.stdout).map_err(|e| {
            format!(
                "nm output for archive shared library `{}` is not UTF-8: {e}",
                shared_object.display()
            )
        })?;
        for line in symbols.lines() {
            let mut it = line.split_ascii_whitespace();
            let Some(symbol) = it.next() else { continue };
            // POSIX format second field = type letter (W/w = weak function, V/v = weak object,
            // u = GNU unique (inline variables/local statics intended as COMDAT; merged by native
            // static linking, always RTLD_LOCAL in glibc dynamic linking—no cross-archive ambiguity);
            // everything else counts as strong)
            let weak = it
                .next()
                .is_some_and(|t| t.starts_with(['W', 'w', 'V', 'v', 'u']));
            std::ffi::CString::new(symbol).map_err(|_| {
                format!(
                    "Archive shared library `{}` exports an illegal symbol name containing NUL",
                    shared_object.display()
                )
            })?;
            match owners.entry(symbol.to_owned()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert((shared_object.clone(), weak));
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    let (prev_path, prev_weak) = e.get().clone();
                    let strongs = usize::from(!prev_weak) + usize::from(!weak);
                    if strongs >= 2 {
                        return Err(format!(
                            "Static archive exported symbol `{symbol}` is defined by both `{}` and `{}` \
                             (two strong definitions); runtime dlsym resolution would depend on load order; \
                             M5.1 refuses to guess native linker order",
                            prev_path.display(),
                            shared_object.display()
                        ));
                    }
                    // All-weak (first wins) or exactly one strong (strong wins over weak): native
                    // link semantics silently resolve the same way—the strong definition enters the owner table
                    if prev_weak && !weak {
                        e.insert((shared_object.clone(), weak));
                    }
                }
            }
        }
    }
    Ok(())
}

fn find_archive(filename: &str, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    search_dirs
        .iter()
        .map(|dir| dir.join(filename))
        .find(|path| path.is_file())
}

fn materialize_for_target_in(
    archive: &Path,
    cache_dir: &Path,
    target: &str,
    cc: &Path,
    extra_libs: &[Box<str>],
    linker: Option<&mut crate::lower::linker::Linker<'_>>,
) -> Result<PathBuf, String> {
    let bytes = std::fs::read(archive).map_err(|e| {
        format!(
            "Failed to read static native archive `{}`: {e}",
            archive.display()
        )
    })?;
    if bytes.starts_with(b"!<thin>\n") {
        return Err(format!(
            "Rejecting thin static archive `{}`: archive bytes do not contain member objects, so it cannot be used as a complete-content hash cache key",
            archive.display()
        ));
    }
    if !bytes.starts_with(b"!<arch>\n") {
        return Err(format!(
            "`{}` is not a supported Unix ar archive",
            archive.display()
        ));
    }
    // Lifecycle-section partition: the .init_array/.fini_array family is **allowed** -- loader
    // DT_INIT_ARRAY semantics are a native process-startup constructor, as used by aws-lc's
    // do_library_init and mimalloc's mi_process_attach. mirvm never dlcloses, so fini has no
    // observable side.
    // Legacy `.init`/`.fini` sections are still **rejected**: that is the old gcc trick of
    // injecting bare function bodies into the init frame, with no frame discipline and fragile
    // cross-toolchain execution semantics (measured in-repo as SIGSEGV during dlopen). Real
    // workloads do not need it (recent C libraries all use constructor attributes), so a loud
    // rejection with a clear diagnosis beats falsely claiming support.
    reject_legacy_init_sections(archive)?;
    let cc_identity = compiler_identity(cc)?;
    // extra_libs (crate-graph system dynamic libraries `-l<name>`) enter both the cache key and the
    // cc link line: a changed list must change the cache slot, so an old closure cannot be reused.
    let extra_flags: Vec<String> = extra_libs.iter().map(|n| format!("-l{n}")).collect();
    let link_flags = LINK_PREFIX
        .iter()
        .chain(LINK_SUFFIX)
        .chain(NATIVE_RUNTIME_WRAP_FLAGS)
        .copied()
        .chain(extra_flags.iter().map(|s| s.as_str()))
        .collect::<Vec<_>>()
        .join("\0");
    let hash = content_hash([
        CACHE_FORMAT_VERSION,
        link_flags.as_bytes(),
        target.as_bytes(),
        &cc_identity,
        NATIVE_RUNTIME_BRIDGE_ASM.as_bytes(),
        &bytes,
    ]);
    std::fs::create_dir_all(cache_dir).map_err(|e| {
        format!(
            "Failed to create native archive cache directory `{}`: {e}",
            cache_dir.display()
        )
    })?;
    let so = cache_dir.join(format!("{hash}.so"));
    if so.exists() {
        return Ok(so);
    }
    let native_runtime_bridge = native_runtime_bridge_object(cache_dir, target, cc, &cc_identity)?;

    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = cache_dir.join(format!("{hash}.so.tmp.{}.{serial}", std::process::id()));
    let output = Command::new(cc)
        .args(LINK_PREFIX)
        .arg(archive)
        .arg(LINK_SUFFIX[0])
        .arg(&native_runtime_bridge)
        .args(NATIVE_RUNTIME_WRAP_FLAGS)
        .args(&LINK_SUFFIX[1..])
        .args(&extra_flags)
        .arg("-o")
        .arg(&tmp)
        .output()
        .map_err(|e| {
            format!(
                "Failed to launch cc to convert `{}`: {e}",
                archive.display()
            )
        })?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        // "Symbols in rlib" rescue chain: undefined ∩ crate-graph rlib exported fn => inject
        // hidden P1-entry trampolines and relink. If the rescue fails (no intersection /
        // non-derivable signature / relink still fails), take the original error path with
        // identical diagnostics.
        if let Some(linker) = linker
            && let Some(so) = rescue_with_rlib_symbols(
                archive,
                cache_dir,
                target,
                cc,
                &extra_flags,
                &bytes,
                &cc_identity,
                &link_flags,
                &native_runtime_bridge,
                linker,
            )?
        {
            return Ok(so);
        }
        return Err(format!(
            "Static native archive `{}` cannot be safely converted to a shared library (requires ELF PIC, dependencies closed within this archive):\n{}{}",
            archive.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    std::fs::rename(&tmp, &so).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!(
            "Atomic publish of native archive cache `{}` failed: {e}",
            so.display()
        )
    })?;
    Ok(so)
}

fn native_runtime_bridge_object(
    cache_dir: &Path,
    target: &str,
    cc: &Path,
    cc_identity: &[u8],
) -> Result<PathBuf, String> {
    let hash = content_hash([
        CACHE_FORMAT_VERSION,
        b"native-runtime-bridge",
        target.as_bytes(),
        cc_identity,
        NATIVE_RUNTIME_BRIDGE_ASM.as_bytes(),
    ]);
    let object = cache_dir.join(format!("{hash}.native-runtime.o"));
    if object.exists() {
        return Ok(object);
    }

    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let source = cache_dir.join(format!(
        "{hash}.native-runtime.s.tmp.{}.{serial}",
        std::process::id()
    ));
    let temporary = cache_dir.join(format!(
        "{hash}.native-runtime.o.tmp.{}.{serial}",
        std::process::id()
    ));
    std::fs::write(&source, NATIVE_RUNTIME_BRIDGE_ASM).map_err(|e| {
        format!(
            "Writing native runtime bridge assembly `{}` failed: {e}",
            source.display()
        )
    })?;
    let output = Command::new(cc)
        .args(["-x", "assembler", "-fPIC", "-c"])
        .arg(&source)
        .arg("-o")
        .arg(&temporary)
        .output()
        .map_err(|e| format!("Failed to launch cc to assemble native runtime bridge: {e}"))?;
    let _ = std::fs::remove_file(&source);
    if !output.status.success() {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "cc assembly of native runtime bridge failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    std::fs::rename(&temporary, &object).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!(
            "Atomic publish of native runtime bridge `{}` failed: {e}",
            object.display()
        )
    })?;
    Ok(object)
}

/// "Symbols in rlib" rescue chain for a first link that failed on undefined symbols:
/// statically enumerate the archive's SHN_UNDEF symbols ∩ crate-graph rlib exported-fn set
/// (`Linker::exported_defs`, the same source as the native final-link symbol set). For each
/// function in the intersection, budget P1 executable entries, emit `.hidden` trampolines, and
/// merge them into the relink. Returns None if the rescue cannot apply (no intersection /
/// non-derivable signature / relink still fails); the caller then takes the original error path.
/// Err is a materialization-time diagnosis (enumeration or trampoline assembly failed), under the
/// same loud-rejection discipline as `-z defs`.
#[allow(clippy::too_many_arguments)]
fn rescue_with_rlib_symbols(
    archive: &Path,
    cache_dir: &Path,
    target: &str,
    cc: &Path,
    extra_flags: &[String],
    archive_bytes: &[u8],
    cc_identity: &[u8],
    link_flags: &str,
    native_runtime_bridge: &Path,
    linker: &mut crate::lower::linker::Linker<'_>,
) -> Result<Option<PathBuf>, String> {
    use rustc_span::Symbol;
    let undefs = crate::elfsym::archive_undefined_symbols(&archive.display().to_string())?;
    if undefs.is_empty() {
        return Ok(None);
    }
    // Intersect with rlib export set (key names unified by canonical_link_name stripping the \x01 prefix family)
    let mut hit: Vec<(Box<str>, rustc_middle::ty::Instance<'_>)> = Vec::new();
    {
        let exports = linker.exported_defs();
        for name in &undefs {
            let canon = crate::lower::ffi_sig::canonical_link_name(name);
            if let Some(&(inst, _is_weak)) = exports.get(&Symbol::intern(canon)) {
                hit.push((canon.into(), inst));
            }
        }
    }
    if crate::options::get().c2_debug {
        eprintln!("c2-debug: undefs={undefs:?} hit={}", hit.len());
    }
    if hit.is_empty() {
        return Ok(None);
    }
    // Budget P1 entries (signature derivability is required; non-derivable = no thunk ABI, hand back to original error path)
    let mut pairs: Vec<(Box<str>, u64)> = Vec::with_capacity(hit.len());
    for (name, inst) in hit {
        if linker.entry_ffi_sig(inst).is_none() {
            return Ok(None);
        }
        let addr = linker
            .fn_entry_addr(inst)
            .map_err(|e| format!("Budgeting P1 entry for rlib symbol `{name}` failed: {e}"))?;
        pairs.push((name, addr));
    }
    pairs.sort();
    // Hidden trampoline assembly. The bridge only references hidden data slots; each Engine writes
    // its own P1 closure address into its copy of the .so, so the artifact does not bake in a fixed
    // runtime address.
    let mut asm = String::from(".intel_syntax noprefix\n");
    let mut slots = std::collections::BTreeSet::new();
    for (name, addr) in &pairs {
        use std::fmt::Write as _;
        let slot = crate::vm::ir::native_entry_slot_name(crate::vm::ir::LinkAddr(*addr));
        let _ = writeln!(asm, ".globl {name}");
        let _ = writeln!(asm, ".hidden {name}");
        let _ = writeln!(asm, ".type {name},@function");
        let _ = writeln!(asm, "{name}:");
        let _ = writeln!(asm, "    jmp QWORD PTR [rip + {slot}]");
        slots.insert(slot);
    }
    if !slots.is_empty() {
        asm.push_str(".pushsection .data.mirvm_p1,\"aw\",@progbits\n.balign 8\n");
        for slot in slots {
            use std::fmt::Write as _;
            let _ = writeln!(asm, ".globl {slot}");
            let _ = writeln!(asm, ".hidden {slot}");
            let _ = writeln!(asm, ".type {slot},@object");
            let _ = writeln!(asm, ".size {slot},8");
            let _ = writeln!(asm, "{slot}:");
            let _ = writeln!(asm, "    .quad 0");
        }
        asm.push_str(".popsection\n");
    }
    // Cache key = first-link key fields + inject pairs (module-specific; P1 code addresses are stable across processes, so same module always hits)
    let mut inject_key: Vec<u8> = Vec::new();
    for (name, addr) in &pairs {
        inject_key.extend_from_slice(name.as_bytes());
        inject_key.push(0);
        inject_key.extend_from_slice(&addr.to_le_bytes());
    }
    let hash = content_hash([
        CACHE_FORMAT_VERSION,
        b"rlib-inject",
        link_flags.as_bytes(),
        target.as_bytes(),
        cc_identity,
        archive_bytes,
        &inject_key,
    ]);
    let so = cache_dir.join(format!("{hash}.so"));
    if so.exists() {
        return Ok(Some(so));
    }
    // Assemble trampoline object (same cc path as native_archive conversion)
    let s_path = cache_dir.join(format!("{hash}.s"));
    std::fs::write(&s_path, &asm)
        .map_err(|e| format!("Writing rlib trampoline assembly `{s_path:?}` failed: {e}"))?;
    let o_path = cache_dir.join(format!("{hash}.tramp.o"));
    let st = Command::new(cc)
        .arg("-c")
        .arg("-o")
        .arg(&o_path)
        .arg(&s_path)
        .status()
        .map_err(|e| {
            format!(
                "Failed to launch cc to assemble rlib trampoline (is cc missing from PATH?): {e}"
            )
        })?;
    if !st.success() {
        return Err(format!(
            "cc assembly of rlib trampoline failed (status={st})"
        ));
    }
    // Relink: trampoline object placed after archive so its defined symbols bind unresolved references inside the archive
    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = cache_dir.join(format!("{hash}.so.tmp.{}.{serial}", std::process::id()));
    let output = Command::new(cc)
        .args(LINK_PREFIX)
        .arg(archive)
        .arg(&o_path)
        .arg(LINK_SUFFIX[0])
        .arg(native_runtime_bridge)
        .args(NATIVE_RUNTIME_WRAP_FLAGS)
        .args(&LINK_SUFFIX[1..])
        .args(extra_flags)
        .arg("-o")
        .arg(&tmp)
        .output()
        .map_err(|e| {
            format!(
                "Failed to launch cc for relink `{}`: {e}",
                archive.display()
            )
        })?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(None);
    }
    std::fs::rename(&tmp, &so).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!(
            "Atomic publish of native archive cache `{}` failed: {e}",
            so.display()
        )
    })?;
    Ok(Some(so))
}

fn compiler_identity(cc: &Path) -> Result<Vec<u8>, String> {
    let version = Command::new(cc)
        .arg("--version")
        .output()
        .map_err(|e| format!("Cannot query C compiler `{}` version: {e}", cc.display()))?;
    if !version.status.success() {
        return Err(format!(
            "Querying C compiler `{}` version failed: {}",
            cc.display(),
            String::from_utf8_lossy(&version.stderr)
        ));
    }
    let machine = Command::new(cc)
        .arg("-dumpmachine")
        .output()
        .map_err(|e| format!("Cannot query C compiler `{}` target: {e}", cc.display()))?;
    if !machine.status.success() {
        return Err(format!(
            "Querying C compiler `{}` target failed: {}",
            cc.display(),
            String::from_utf8_lossy(&machine.stderr)
        ));
    }
    let first_line = version
        .stdout
        .split(|&b| b == b'\n')
        .next()
        .unwrap_or_default();
    let mut identity = first_line.to_vec();
    identity.push(0);
    identity.extend_from_slice(machine.stdout.trim_ascii());
    Ok(identity)
}

fn reject_legacy_init_sections(archive: &Path) -> Result<(), String> {
    let output = Command::new("readelf")
        .args(["--section-headers", "--wide"])
        .arg(archive)
        .output()
        .map_err(|e| {
            format!(
                "Cannot inspect legacy init sections of static native archive `{}` (failed to launch readelf): {e}",
                archive.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "Cannot inspect legacy init sections of static native archive `{}` (readelf failed):\n{}{}",
            archive.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let sections = String::from_utf8_lossy(&output.stdout);
    let has_legacy = sections.lines().filter_map(readelf_section_name).any(|n| {
        n == ".init"
            || n == ".fini"
            || n.strip_prefix(".init.")
                .is_some_and(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
            || n.strip_prefix(".fini.")
                .is_some_and(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
    });
    if has_legacy {
        return Err(format!(
            "Rejecting static native archive `{}` with legacy `.init`/`.fini` sections: \
             the old gcc trick of injecting bare function bodies has unreliable execution semantics (.init_array family is allowed)",
            archive.display()
        ));
    }
    Ok(())
}

fn readelf_section_name(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if !line.starts_with('[') {
        return None;
    }
    let close = line.find(']')?;
    line[close + 1..].split_ascii_whitespace().next()
}

fn content_hash<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut left = 0xcbf2_9ce4_8422_2325_u64;
    let mut right = 0x6c62_272e_07bb_0142_u64;
    for part in parts {
        for &byte in part {
            left ^= u64::from(byte);
            left = left.wrapping_mul(0x0000_0100_0000_01b3);
            right ^= u64::from(byte).wrapping_add(left.rotate_left(17));
            right = right.wrapping_mul(0x9e37_79b1_85eb_ca87);
        }
        left ^= 0xff;
        right ^= left.rotate_right(11);
    }
    format!("{left:016x}{right:016x}")
}

#[cfg(test)]
mod tests;
