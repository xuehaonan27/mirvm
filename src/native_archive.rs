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
/// Closure baseline = the set of system libraries that std brings to the guest's final link via `#[link]`
/// (glibc: m/dl/pthread/rt/util/gcc_s; these are always present in native semantics, so rustc C static archives
/// can reference their symbols directly—e.g. libsqlite3's FTS5 references libm `log`, and the pthread family,
/// proven by corpus batch 3 rusqlite).
/// They become DT_NEEDED entries of the produced .so and are resolved by the host environment at dlopen time;
/// `-z defs` continues to loudly reject undefined references outside this set (cross-archive / guest symbols),
/// so closure discipline is not relaxed.
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

/// Collect system dynamic library names propagated through the crate graph (proven by corpus batch 7 c_libgit2):
/// `-sys` crates' `cargo:rustc-link-lib` only writes rlib metadata—the native final link command line has these `-l`
/// entries, while the metadata driver (the bin command line) does not. When C objects in a static archive reference
/// these libraries (e.g. libgit2.a's crc32/deflate → libz-sys's `z`), the closure link line must also include them—
/// same collection scope as `system_dylib_preload` (lower's RTLD_GLOBAL preload), consumed in both places.
/// Static { bundle: None | Some(true) } are true static archives that go entirely into the rlib (handled by the
/// archive path in the loop above, skipped here); Framework / LinkArg / wasm are out of scope for this slice.
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
        env!("MIRVM_HOST"),
        Path::new("cc"),
        &[],
        None,
    )
}

/// Collect the current crate graph's Static native libraries and convert each independent archive into a `.so`.
///
/// Only a constrained vertical slice for Linux/ELF is implemented. Each archive is linked independently with `-z defs`,
/// so cross-archive dependencies, dependency ordering, and non-PIC relocations fail loudly; no generic native link plan is guessed.
/// C2: on conversion failure, enter the "symbols in rlib" rescue chain
/// (undefined ∩ crate-graph rlib exported fn ⇒ inject hidden P1-entry trampolines and relink;
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
    let cache = crate::sysroot::cache_dir().join("native-archives");
    let mut shared_objects = Vec::<PathBuf>::new();
    // Crate-graph system dynamic libraries (proven by c_libgit2: when a static archive's C objects reference
    // `-l` library symbols propagated via metadata, the closure link line must also include them;
    // same list as lower's RTLD_GLOBAL preload)
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
            if target != env!("MIRVM_HOST")
                || sess.target.os != Os::Linux
                || sess.target.binary_format != BinaryFormat::Elf
            {
                return Err(format!(
                    "crate `{crate_name}`'s Static native library `{}` can only be handled by the current host \
                     Linux/ELF archive-loading slice (host: {}, current target: {target})",
                    lib.name,
                    env!("MIRVM_HOST")
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
/// **share names across archives** (resolution would depend on load order; M5.1 refuses to guess native linker order).
///
/// Weak-semantic correction (2026-07-18, proven by corpus batch 10 c_risc0_run): duplicate symbols are handled
/// according to native link semantics—**all-weak definitions are allowed** (weak/COMDAT first-wins; load order
/// is isomorphic to crate-graph order and native link order; the three risc0 `-sys` crates each export the
/// C++ sized-delete `_ZdlPvS_` COMDAT, which belongs to this family); **exactly one strong definition is allowed**
/// (strong wins over weak, same silent resolution as native); **≥2 strong definitions remain rejected**
/// (native would already be a link error, so we also reject loudly).
///
/// Collisions with **existing RTLD_DEFAULT definitions** used to be rejected alongside (1); since dynsym archive handles
/// now take priority, they are no longer rejected: resolution order is ① hidden fallback table → ② archive handle
/// (link order) → ③ global dlsym. Objects linked by the guest (hidden or dynsym-visible) always beat host-process
/// libraries of the same name—native link-time binding semantics (psm's `rust_psm_on_stack` vs the embedded copy
/// in the host `librustc_driver`, proven by corpus batch 6 c_polars_frame; zstd-sys's ZSTD_* vs libLLVM's embedded
/// library are the same family). Known residual: **intra-archive** cross-references to colliding symbols still go through
/// the dynamic linker's global order (cannot be mirrored; no such shape exists in the corpus—the four psm symbols are
/// only called from the Rust side with no internal cross-references).
///
/// Hidden symbols that do not enter .dynsym (carried by the .symtab fallback table) deliberately skip all collision checks:
/// in resolution order they always precede the global scope, so collisions already resolve to the archive, leaving no ambiguity to reject.
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
    let bytes = std::fs::read(archive)
        .map_err(|e| format!("Failed to read static native archive `{}`: {e}", archive.display()))?;
    if bytes.starts_with(b"!<thin>\n") {
        return Err(format!(
            "Rejecting thin static archive `{}`: archive bytes do not contain member objects, so it cannot be used as a complete-content hash cache key",
            archive.display()
        ));
    }
    if !bytes.starts_with(b"!<arch>\n") {
        return Err(format!("`{}` is not a supported Unix ar archive", archive.display()));
    }
    // Lifecycle-section partition (§7.8): .init_array/.fini_array family **allowed**—loader
    // DT_INIT_ARRAY semantics = native process-startup constructor (proven by aws-lc do_library_init /
    // mimalloc mi_process_attach; mirvm never dlcloses, so fini has no observable side);
    // legacy `.init`/`.fini` sections remain **rejected**—that is the old gcc trick of injecting
    // bare function bodies into the init frame, with no frame discipline and fragile cross-toolchain
    // execution semantics (measured in-repo as SIGSEGV during dlopen);
    // real workloads (recent C libraries all use constructor attributes) do not need it,
    // so we prefer a loud rejection with a clear diagnosis over falsely claiming support.
    reject_legacy_init_sections(archive)?;
    let cc_identity = compiler_identity(cc)?;
    // extra_libs (crate-graph system dynamic libraries `-l<name>`) enter both the cache key and the cc link line—
    // list changes must change cache slots, so old closures cannot be falsely reused (the key discipline fixed by c_libgit2)
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
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("Failed to create native archive cache directory `{}`: {e}", cache_dir.display()))?;
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
        .map_err(|e| format!("Failed to launch cc to convert `{}`: {e}", archive.display()))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        // C2: "symbols in rlib" rescue chain (designs/c2-rlib-symbols-design.md §2)—
        // undefined ∩ crate-graph rlib exported fn ⇒ inject hidden P1-entry trampolines and relink;
        // if rescue fails (no intersection / non-derivable signature / relink still fails), take the original error path with byte-identical diagnostics
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
        format!("Atomic publish of native archive cache `{}` failed: {e}", so.display())
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

/// C2 "symbols in rlib" rescue chain (designs/c2-rlib-symbols-design.md §2):
/// After the first link fails, statically enumerate the archive's SHN_UNDEF symbols ∩ crate-graph rlib exported-fn set
/// (`Linker::exported_defs`, same source as the native final-link symbol set)—for fn in the intersection,
/// budget P1 executable entries, emit `.hidden` trampolines, and merge them into the relink. Returns None if rescue fails
/// (no intersection / non-derivable signature / relink still fails); caller takes the original error path.
/// Err = materialization-time diagnosis (enumeration / trampoline assembly failed—same loud-rejection discipline as `-z defs`).
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
    if std::env::var_os("MIRVM_C2_DEBUG").is_some() {
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
    // Hidden trampoline .s (same shape as C7). The bridge only references hidden data slots; each Engine writes
    // its own P1 closure address into its copy of the .so, so the artifact no longer bakes in a fixed runtime address.
    let mut asm = String::from(".intel_syntax noprefix\n");
    let mut slots = std::collections::BTreeSet::new();
    for (name, addr) in &pairs {
        use std::fmt::Write as _;
        let slot =
            crate::vm::engine::ir::native_entry_slot_name(crate::vm::engine::ir::LinkAddr(*addr));
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
        .map_err(|e| format!("Failed to launch cc to assemble rlib trampoline (is cc missing from PATH?): {e}"))?;
    if !st.success() {
        return Err(format!("cc assembly of rlib trampoline failed (status={st})"));
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
        .map_err(|e| format!("Failed to launch cc for relink `{}`: {e}", archive.display()))?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(None);
    }
    std::fs::rename(&tmp, &so).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Atomic publish of native archive cache `{}` failed: {e}", so.display())
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
mod tests {
    use std::ffi::CString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{materialize_for_target_in, materialize_in, reject_symbol_ambiguity};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
    static SIGNAL_SIGNUM: AtomicU64 = AtomicU64::new(0);
    static SIGNAL_HANDLER: AtomicU64 = AtomicU64::new(0);
    static SIGNAL_OWNER: AtomicU64 = AtomicU64::new(0);
    static SIGACTION_SIGNUM: AtomicU64 = AtomicU64::new(0);
    static SIGACTION_OWNER: AtomicU64 = AtomicU64::new(0);
    static RAISE_SIGNUM: AtomicU64 = AtomicU64::new(0);
    static RAISE_OWNER: AtomicU64 = AtomicU64::new(0);

    extern "C" fn test_native_signal(signum: i32, handler: usize, owner: u64) -> usize {
        SIGNAL_SIGNUM.store(signum as u64, Ordering::Relaxed);
        SIGNAL_HANDLER.store(handler as u64, Ordering::Relaxed);
        SIGNAL_OWNER.store(owner, Ordering::Relaxed);
        handler
    }

    extern "C" fn test_native_sigaction(
        signum: i32,
        _act: *const libc::c_void,
        _oldact: *mut libc::c_void,
        owner: u64,
    ) -> i32 {
        SIGACTION_SIGNUM.store(signum as u64, Ordering::Relaxed);
        SIGACTION_OWNER.store(owner, Ordering::Relaxed);
        71
    }

    extern "C" fn test_native_raise(signum: i32, owner: u64) -> i32 {
        RAISE_SIGNUM.store(signum as u64, Ordering::Relaxed);
        RAISE_OWNER.store(owner, Ordering::Relaxed);
        73
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mirvm-native-archive-test-{name}-{}-{id}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_archive(dir: &Path, source: &str) -> PathBuf {
        let source_path = dir.join("probe.c");
        let object_path = dir.join("probe.o");
        let archive_path = dir.join("libprobe.a");
        std::fs::write(&source_path, source).unwrap();
        let cc = Command::new("cc")
            .args(["-fPIC", "-c"])
            .arg(&source_path)
            .arg("-o")
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(cc.success());
        let ar = Command::new("ar")
            .arg("crs")
            .arg(&archive_path)
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(ar.success());
        archive_path
    }

    fn make_thin_archive(dir: &Path, source: &str) -> PathBuf {
        let source_path = dir.join("thin_probe.c");
        let object_path = dir.join("thin_probe.o");
        let archive_path = dir.join("libthin_probe.a");
        std::fs::write(&source_path, source).unwrap();
        let cc = Command::new("cc")
            .args(["-fPIC", "-c"])
            .arg(&source_path)
            .arg("-o")
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(cc.success());
        let ar = Command::new("ar")
            .arg("crsT")
            .arg(&archive_path)
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(ar.success());
        archive_path
    }

    fn make_non_pic_archive(dir: &Path, source: &str) -> PathBuf {
        let source_path = dir.join("non_pic_probe.c");
        let object_path = dir.join("non_pic_probe.o");
        let archive_path = dir.join("libnon_pic_probe.a");
        std::fs::write(&source_path, source).unwrap();
        let cc = Command::new("cc")
            // Force an actually absolute text relocation. On x86-64 the default
            // small model emits PC-relative code even without `-fPIC`; -Bsymbolic
            // can resolve that code safely inside the private Engine image.
            .args(["-fno-pic", "-mcmodel=large", "-c"])
            .arg(&source_path)
            .arg("-o")
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(cc.success());
        let ar = Command::new("ar")
            .arg("crs")
            .arg(&archive_path)
            .arg(&object_path)
            .status()
            .unwrap();
        assert!(ar.success());
        archive_path
    }

    fn make_cc_wrapper(dir: &Path, name: &str, version: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version}'; exit 0; fi\nexec cc \"$@\"\n"
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn unreferenced_archive_symbol_is_dlsym_visible() {
        let temp = TempDir::new("tracer");
        let archive = make_archive(
            temp.path(),
            "unsigned long mirvm_archive_probe(void) { return 0x51aUL; }\n",
        );

        let so = materialize_in(&archive, &temp.path().join("cache")).unwrap();
        let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
        let handle = crate::os::dll::open_with_flags(
            &c_so,
            crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
        )
        .unwrap_or_else(|e| panic!("dlopen {} failed: {}", so.display(), e));
        let address = crate::os::dll::sym(handle, c"mirvm_archive_probe");
        assert!(address != 0, "whole-archive did not export tracer symbol");
        let probe: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(address as *const u8) };
        assert_eq!(unsafe { probe() }, 0x51a);
        unsafe { crate::os::dll::close(handle) };
    }

    #[test]
    fn native_signal_calls_receive_the_engine_owner() {
        const OWNER: u64 = 0x0ddc_0ffe_e15e_c7ed;
        const HANDLER: usize = 0x1234_5678;

        let temp = TempDir::new("signal-bridge");
        let archive = make_archive(
            temp.path(),
            "#include <signal.h>\n\
             void *mirvm_call_signal(int signum, void *handler) {\n\
                 return (void *)signal(signum, (void (*)(int))handler);\n\
             }\n\
             int mirvm_call_sigaction(int signum) { return sigaction(signum, 0, 0); }\n\
             int mirvm_call_raise(int signum) { return raise(signum); }\n",
        );

        let so = materialize_in(&archive, &temp.path().join("cache")).unwrap();
        let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
        let handle = crate::os::dll::open_with_flags(
            &c_so,
            crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
        )
        .unwrap_or_else(|e| panic!("dlopen {} failed: {e}", so.display()));
        let bias = crate::os::dll::load_bias(handle).expect("native bridge load bias") as u64;
        let hidden = crate::elfsym::hidden_symtab_values(so.to_str().unwrap()).unwrap();
        let patch = |name: &str, value: u64| {
            let offset = hidden
                .get(name)
                .unwrap_or_else(|| panic!("native bridge has no hidden slot `{name}`"));
            let address = bias
                .checked_add(*offset)
                .expect("native bridge slot address");
            unsafe { (address as *mut u64).write(value) };
        };
        patch("__mirvm_signal_owner", OWNER);
        patch(
            "__mirvm_signal_target",
            test_native_signal as *const () as usize as u64,
        );
        patch(
            "__mirvm_sigaction_target",
            test_native_sigaction as *const () as usize as u64,
        );
        patch(
            "__mirvm_raise_target",
            test_native_raise as *const () as usize as u64,
        );

        let signal: unsafe extern "C" fn(i32, usize) -> usize =
            unsafe { std::mem::transmute(crate::os::dll::sym(handle, c"mirvm_call_signal")) };
        let sigaction: unsafe extern "C" fn(i32) -> i32 =
            unsafe { std::mem::transmute(crate::os::dll::sym(handle, c"mirvm_call_sigaction")) };
        let raise: unsafe extern "C" fn(i32) -> i32 =
            unsafe { std::mem::transmute(crate::os::dll::sym(handle, c"mirvm_call_raise")) };

        assert_eq!(unsafe { signal(1234, HANDLER) }, HANDLER);
        assert_eq!(SIGNAL_SIGNUM.load(Ordering::Relaxed), 1234);
        assert_eq!(SIGNAL_HANDLER.load(Ordering::Relaxed), HANDLER as u64);
        assert_eq!(SIGNAL_OWNER.load(Ordering::Relaxed), OWNER);
        assert_eq!(unsafe { sigaction(1235) }, 71);
        assert_eq!(SIGACTION_SIGNUM.load(Ordering::Relaxed), 1235);
        assert_eq!(SIGACTION_OWNER.load(Ordering::Relaxed), OWNER);
        assert_eq!(unsafe { raise(1236) }, 73);
        assert_eq!(RAISE_SIGNUM.load(Ordering::Relaxed), 1236);
        assert_eq!(RAISE_OWNER.load(Ordering::Relaxed), OWNER);

        unsafe { crate::os::dll::close(handle) };
    }

    #[test]
    fn thin_archive_is_rejected_because_its_content_hash_is_incomplete() {
        let temp = TempDir::new("thin");
        let archive = make_thin_archive(
            temp.path(),
            "unsigned long mirvm_thin_probe(void) { return 7UL; }\n",
        );

        let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
        assert!(error.contains("thin"), "unexpected diagnostic: {error}");
    }

    #[test]
    fn constructor_runs_at_dlopen_after_lifecycle_guard_is_lifted() {
        // §7.8: lifecycle guard decoded—dlopen's DT_INIT_ARRAY semantics = native process-startup
        // constructor (two proven cases: aws-lc do_library_init / mimalloc mi_process_attach);
        // mirvm never dlcloses, so fini has no observable side (same as native exit being reclaimed by the OS).
        let temp = TempDir::new("constructor-accepted");
        let archive = make_archive(
            temp.path(),
            "static unsigned long mirvm_flag;\n\
             static void boot(void) __attribute__((constructor));\n\
             static void boot(void) { mirvm_flag = 42UL; }\n\
             unsigned long mirvm_constructor_probe(void) { return mirvm_flag; }\n",
        );

        let so = materialize_in(&archive, &temp.path().join("cache")).unwrap();
        let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
        let handle = crate::os::dll::open_with_flags(
            &c_so,
            crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
        )
        .unwrap_or_else(|_| panic!("dlopen {} failed", so.display()));
        let address = crate::os::dll::sym(handle, c"mirvm_constructor_probe");
        assert!(address != 0);
        let probe: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(address as *const u8) };
        assert_eq!(
            unsafe { probe() },
            42,
            "DT_INIT_ARRAY must have already executed at dlopen time (constructor set it to 42)"
        );
        unsafe { crate::os::dll::close(handle) };
    }

    #[test]
    fn legacy_elf_init_and_fini_sections_are_rejected() {
        // §7.8 partition: legacy `.init`/`.fini` sections remain rejected (old gcc trick of injecting
        // bare function bodies, unreliable execution semantics—measured in-repo as SIGSEGV during dlopen);
        // .init_array family is allowed (see constructor_runs_at_dlopen_after_lifecycle_guard_is_lifted).
        let temp = TempDir::new("legacy-init-fini");
        for section in [".init", ".fini"] {
            let dir = temp.path().join(section.trim_start_matches('.'));
            std::fs::create_dir_all(&dir).unwrap();
            let archive = make_archive(
                &dir,
                &format!(
                    "__attribute__((used, section(\"{section}\"))) \
                     void mirvm_lifecycle_hook(void) {{}}\n"
                ),
            );

            let error = materialize_in(&archive, &dir.join("cache")).unwrap_err();
            assert!(
                error.contains("`.init`/`.fini`"),
                "section {section} was not rejected: {error}"
            );
        }
    }

    #[test]
    fn dot_init_in_archive_path_is_not_mistaken_for_a_section() {
        let temp = TempDir::new("section-parser");
        let dir = temp.path().join("ordinary.init.path");
        std::fs::create_dir_all(&dir).unwrap();
        let archive = make_archive(
            &dir,
            "unsigned long mirvm_not_a_constructor(void) { return 23UL; }\n",
        );

        materialize_in(&archive, &dir.join("cache"))
            .expect("`.init` in a path must not be parsed as an ELF section");
    }

    #[test]
    fn cache_key_separates_target_triples() {
        let temp = TempDir::new("target-key");
        let archive = make_archive(
            temp.path(),
            "unsigned long mirvm_target_key_probe(void) { return 11UL; }\n",
        );
        let cache = temp.path().join("cache");

        let first = materialize_for_target_in(
            &archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            Path::new("cc"),
            &[],
            None,
        )
        .unwrap();
        let second = materialize_for_target_in(
            &archive,
            &cache,
            "aarch64-unknown-linux-gnu",
            Path::new("cc"),
            &[],
            None,
        )
        .unwrap();
        assert_ne!(first, second, "target triple must participate in cache key");
    }

    #[test]
    fn unresolved_archive_dependency_fails_during_materialization() {
        let temp = TempDir::new("unresolved");
        let archive = make_archive(
            temp.path(),
            "extern unsigned long mirvm_missing_dependency(void);\n\
             unsigned long mirvm_dependency_probe(void) { return mirvm_missing_dependency(); }\n",
        );

        let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
        assert!(error.contains("dependency"), "unexpected diagnostic: {error}");
        assert!(
            error.contains("mirvm_missing_dependency"),
            "linker detail lost: {error}"
        );
    }

    #[test]
    fn non_pic_archive_fails_during_materialization() {
        let temp = TempDir::new("non-pic");
        let archive = make_non_pic_archive(
            temp.path(),
            "unsigned long mirvm_non_pic_global = 13UL;\n\
             unsigned long mirvm_non_pic_probe(void) { return mirvm_non_pic_global; }\n",
        );

        let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
        assert!(error.contains("PIC"), "unexpected diagnostic: {error}");
        assert!(error.contains("relocation"), "linker detail lost: {error}");
    }

    #[test]
    fn cache_key_separates_c_compiler_identities() {
        let temp = TempDir::new("cc-key");
        let archive = make_archive(
            temp.path(),
            "unsigned long mirvm_cc_key_probe(void) { return 17UL; }\n",
        );
        let cc_a = make_cc_wrapper(temp.path(), "cc-a", "mirvm test cc A");
        let cc_b = make_cc_wrapper(temp.path(), "cc-b", "mirvm test cc B");
        let cache = temp.path().join("cache");

        let first = materialize_for_target_in(
            &archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            &cc_a,
            &[],
            None,
        )
        .unwrap();
        let second = materialize_for_target_in(
            &archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            &cc_b,
            &[],
            None,
        )
        .unwrap();
        assert_ne!(
            first, second,
            "C compiler identity must participate in cache key"
        );
    }

    #[test]
    fn cache_key_separates_extra_libs() {
        let temp = TempDir::new("extra-libs-key");
        let archive = make_archive(
            temp.path(),
            "unsigned long mirvm_extra_libs_probe(void) { return 29UL; }\n",
        );
        let cache = temp.path().join("cache");
        let none: &[Box<str>] = &[];
        let with_m: &[Box<str>] = &["m".into()];
        let first = materialize_for_target_in(
            &archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            Path::new("cc"),
            none,
            None,
        )
        .unwrap();
        let second = materialize_for_target_in(
            &archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            Path::new("cc"),
            with_m,
            None,
        )
        .unwrap();
        assert_ne!(first, second, "extra libs list must participate in cache key");
    }

    #[test]
    fn duplicate_symbols_across_archives_are_rejected_as_link_order_ambiguity() {
        let temp = TempDir::new("duplicate-symbol");
        let first_dir = temp.path().join("first");
        let second_dir = temp.path().join("second");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        let first_archive = make_archive(
            &first_dir,
            "unsigned long mirvm_duplicate_symbol(void) { return 1UL; }\n",
        );
        let second_archive = make_archive(
            &second_dir,
            "unsigned long mirvm_duplicate_symbol(void) { return 2UL; }\n",
        );
        let cache = temp.path().join("cache");
        let first = materialize_for_target_in(
            &first_archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            Path::new("cc"),
            &[],
            None,
        )
        .unwrap();
        let second = materialize_for_target_in(
            &second_archive,
            &cache,
            "x86_64-unknown-linux-gnu",
            Path::new("cc"),
            &[],
            None,
        )
        .unwrap();

        let error = reject_symbol_ambiguity(&[first, second]).unwrap_err();
        assert!(
            error.contains("mirvm_duplicate_symbol"),
            "unexpected diagnostic: {error}"
        );
        assert!(error.contains("order"), "unexpected diagnostic: {error}");
    }

    /// weak/COMDAT semantics (proven by c_risc0_run): duplicate symbols across archives—
    /// all-weak allowed (first wins); exactly one strong + weak allowed (strong wins); two strong remain rejected.
    #[test]
    fn duplicate_weak_symbols_follow_native_link_semantics() {
        let temp = TempDir::new("duplicate-weak");
        let (d1, d2, d3) = (
            temp.path().join("d1"),
            temp.path().join("d2"),
            temp.path().join("d3"),
        );
        for d in [&d1, &d2, &d3] {
            std::fs::create_dir_all(d).unwrap();
        }
        let weak_a = make_archive(
            &d1,
            "__attribute__((weak)) unsigned long mirvm_dup_weak(void) { return 1UL; }\n",
        );
        let weak_b = make_archive(
            &d2,
            "__attribute__((weak)) unsigned long mirvm_dup_weak(void) { return 2UL; }\n",
        );
        let strong_c = make_archive(&d3, "unsigned long mirvm_dup_weak(void) { return 3UL; }\n");
        let cache = temp.path().join("cache");
        let mat = |a: &PathBuf| {
            materialize_for_target_in(
                a,
                &cache,
                "x86_64-unknown-linux-gnu",
                Path::new("cc"),
                &[],
                None,
            )
            .unwrap()
        };
        let (sa, sb, sc) = (mat(&weak_a), mat(&weak_b), mat(&strong_c));
        // All-weak: allowed (native first-wins isomorphic)
        reject_symbol_ambiguity(&[sa.clone(), sb.clone()]).expect("duplicate all-weak names must be allowed");
        // strong + weak: allowed (native strong-wins same resolution)
        reject_symbol_ambiguity(&[sa.clone(), sc.clone()]).expect("strong+weak must be allowed");
        // Two strong: remain rejected (native would already be a link error)
        let strong_d_dir = temp.path().join("d4");
        std::fs::create_dir_all(&strong_d_dir).unwrap();
        let strong_d = make_archive(
            &strong_d_dir,
            "unsigned long mirvm_dup_weak(void) { return 4UL; }\n",
        );
        let sd = mat(&strong_d);
        reject_symbol_ambiguity(&[sc, sd]).unwrap_err();
    }

    #[test]
    fn symbol_already_in_process_is_accepted_under_handle_first_resolution() {
        // New semantics after dynsym archive-handle priority: collisions between archive-exported symbols
        // and existing process definitions (here deliberately using malloc, which every process defines)
        // are no longer rejected—the archive handle always resolves first, reproducible as native link-time binding
        // (the guest's own objects always beat host libraries of the same name).
        let temp = TempDir::new("process-symbol");
        let archive = make_archive(
            temp.path(),
            "void *malloc(unsigned long size) { (void)size; return (void *)0; }\n",
        );
        let shared_object = materialize_in(&archive, &temp.path().join("cache")).unwrap();

        reject_symbol_ambiguity(&[shared_object]).unwrap();
    }
}
