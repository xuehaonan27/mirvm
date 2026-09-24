//! Static native archive loader: materialize a constraint-checked PIC `.a` into a dlopen-able image.
//!
//! What the archive has to satisfy, and what each link line looks like, is read out of the bytes and
//! asked of the platform rather than of a tool: `nm`, `readelf` and the linker flags all spell
//! things the object format and the toolchain do, and this build meets two of each.

/// Why a native archive could not be turned into something the engine can load.
///
/// The classes follow what a caller can do: `Missing` (the crate's library is not in the search
/// path), `Malformed` (the bytes are not the shape, or the symbol names are unusable),
/// `Unsupported` (well formed but outside what mirvm handles: another host's slice, a thin archive,
/// an `+/-export-symbols` modifier, a non-PIC member, legacy `.init`/`.fini`), `Tool` (an external
/// `cc` ran and failed, with its output as the detail), `Ambiguous` (two strong definitions of one
/// symbol, where resolution would depend on native link order and mirvm refuses to guess), and `Io`
/// (a filesystem or exec step failed, carrying the `io::Error` as its source).
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub(crate) enum Error {
    #[error("{detail}")]
    Missing { detail: String },

    #[error("{detail}")]
    Malformed { detail: String },

    #[error("{detail}")]
    Unsupported { detail: String },

    #[error("{detail}")]
    Tool { detail: String },

    #[error("{detail}")]
    Ambiguous { detail: String },

    /// The archive's symbol table could not be read.
    #[error(transparent)]
    Symtab(#[from] super::symtab::Error),

    #[error("{detail}: {source}")]
    Io {
        detail: String,
        #[serde(skip)]
        #[source]
        source: std::io::Error,
    },
}

crate::diag_codes! {
    Error: Native => {
        Missing => "archive.missing",
        Malformed => "archive.malformed",
        Unsupported => "archive.unsupported",
        Tool => "archive.tool",
        Ambiguous => "archive.ambiguous",
        Symtab => "archive.symtab",
        Io => "archive.io",
    }
}

impl Error {
    fn missing(detail: impl Into<String>) -> Self {
        Error::Missing {
            detail: detail.into(),
        }
    }

    fn malformed(detail: impl Into<String>) -> Self {
        Error::Malformed {
            detail: detail.into(),
        }
    }

    fn unsupported(detail: impl Into<String>) -> Self {
        Error::Unsupported {
            detail: detail.into(),
        }
    }

    fn tool(detail: impl Into<String>) -> Self {
        Error::Tool {
            detail: detail.into(),
        }
    }

    fn ambiguous(detail: impl Into<String>) -> Self {
        Error::Ambiguous {
            detail: detail.into(),
        }
    }

    fn io(detail: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            detail: detail.into(),
            source,
        }
    }
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use rustc_hir::attrs::NativeLibKind;
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_middle::ty::TyCtxt;
use rustc_session::search_paths::PathKind;
use rustc_target::spec::{BinaryFormat, Os};

const CACHE_FORMAT_VERSION: &[u8] = b"mirvm-native-archive-v4";
/// Closure baseline: the system libraries std contributes to the guest's final link via `#[link]`,
/// which a C static archive may therefore reference directly -- libsqlite3's FTS5 references libm
/// `log` and the pthread family, for instance. They become the image's `DT_NEEDED` entries and are
/// resolved by the host environment at `dlopen` time. Which libraries those are, and whether this
/// platform can refuse an undefined reference outside them at all, is `crate::os::linker`'s: the
/// two platforms answer both differently, and only one of them can refuse.
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
pub(crate) fn materialize_in(archive: &Path, cache_dir: &Path) -> Result<PathBuf, Error> {
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
) -> Result<Vec<Box<str>>, Error> {
    let sess = tcx.sess;
    let search_dirs: Vec<_> = sess
        .target_filesearch()
        .cli_search_paths(PathKind::Native)
        .map(|path| path.dir.clone())
        .collect();
    let target = sess.opts.target_triple.tuple();
    let cache = crate::store::NATIVE_ARCHIVES.dir();
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
                return Err(Error::unsupported(format!(
                    "crate `{crate_name}`'s Static native library `{}` can only be handled by the current host \
                     Linux/ELF archive-loading slice (host: {}, current target: {target})",
                    lib.name,
                    crate::options::build::HOST
                )));
            }
            if export_symbols.is_some() {
                return Err(Error::unsupported(format!(
                    "crate `{crate_name}`'s Static native library `{}` uses the \
                     `+/-export-symbols` modifier; M5.1 archive loading has not defined its `.so` equivalent semantics",
                    lib.name
                )));
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
                Error::missing(format!(
                    "Cannot find crate `{crate_name}`'s Static native library `{}` (file `{filename}`); \
                     rustc native search paths: [{}]",
                    lib.name, searched
                ))
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
pub(super) fn reject_symbol_ambiguity(shared_objects: &[PathBuf]) -> Result<(), Error> {
    let mut owners = HashMap::<String, (PathBuf, bool)>::new();
    for shared_object in shared_objects {
        // Read here rather than by `nm`: the tool's name, its flags and the type letters it prints
        // are each the object format's, and this build meets two of those.
        for export in crate::native::symtab::object_exports(
            &shared_object.to_string_lossy(),
            crate::os::dll::OBJECT_FORMAT,
        )? {
            let weak = export.weak;
            match owners.entry(export.name.into_string()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert((shared_object.clone(), weak));
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let (previous, previous_weak) = entry.get().clone();
                    let strongs = usize::from(!previous_weak) + usize::from(!weak);
                    if strongs >= 2 {
                        let symbol = entry.key().clone();
                        return Err(Error::ambiguous(format!(
                            "Static archive exported symbol `{symbol}` is defined by both `{}` and `{}` \
                             (two strong definitions); runtime dlsym resolution would depend on load order; \
                             M5.1 refuses to guess native linker order",
                            previous.display(),
                            shared_object.display()
                        )));
                    }
                    // All-weak (the first wins) or exactly one strong (the strong definition wins
                    // over the weak one): native link semantics resolve both the same way, so the
                    // owner becomes whichever definition would have won.
                    if previous_weak && !weak {
                        entry.insert((shared_object.clone(), weak));
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

pub(super) fn materialize_for_target_in(
    archive: &Path,
    cache_dir: &Path,
    target: &str,
    cc: &Path,
    extra_libs: &[Box<str>],
    linker: Option<&mut crate::lower::linker::Linker<'_>>,
) -> Result<PathBuf, Error> {
    let bytes = std::fs::read(archive).map_err(|e| {
        Error::io(
            format!(
                "cannot read the static native archive `{}`",
                archive.display()
            ),
            e,
        )
    })?;
    if bytes.starts_with(b"!<thin>\n") {
        return Err(Error::unsupported(format!(
            "Rejecting thin static archive `{}`: archive bytes do not contain member objects, so it cannot be used as a complete-content hash cache key",
            archive.display()
        )));
    }
    if !bytes.starts_with(b"!<arch>\n") {
        return Err(Error::malformed(format!(
            "`{}` is not a supported Unix ar archive",
            archive.display()
        )));
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
    // Asked for before anything is written: a platform whose linker cannot redirect the calls an
    // image must not reach directly has to fail here rather than after building an object the link
    // cannot use.
    let interpose = crate::os::linker::interpose_args(crate::vm::interpose::INTERPOSED_CALLS)
        .ok_or_else(|| {
            Error::unsupported(
                "cannot link a native archive image: this platform's linker has no way to redirect \
                 the runtime calls the image must not reach directly, so the image would silently \
                 keep calls the engine has to own",
            )
        })?;
    std::fs::create_dir_all(cache_dir).map_err(|e| {
        Error::io(
            format!(
                "cannot create the native archive cache directory `{}`",
                cache_dir.display()
            ),
            e,
        )
    })?;
    // The bridge is an input of this link like any other, so its content belongs in the cache key.
    // Its name *is* that content: the artifact is addressed by the hash of what it was built from.
    let native_runtime_bridge =
        crate::native::bridge::artifact(cache_dir, cc, &cc_identity).map_err(Error::tool)?;
    let bridge_name = native_runtime_bridge
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let link = native_archive_link(
        archive,
        &[],
        &native_runtime_bridge,
        &interpose,
        &extra_flags,
    );
    // The key is the line's *shape*: the two paths in it are already in the key as content -- the
    // archive's bytes and the bridge's name -- and a placeholder keeps a path from splitting a slot
    // the content says is the same.
    let shape = native_archive_link(
        Path::new("<archive>"),
        &[],
        Path::new("<bridge>"),
        &interpose,
        &extra_flags,
    )
    .iter()
    .map(|argument| argument.to_string_lossy().into_owned())
    .collect::<Vec<_>>()
    .join("\0");
    let hash = content_hash([
        CACHE_FORMAT_VERSION,
        shape.as_bytes(),
        target.as_bytes(),
        &cc_identity,
        bridge_name.as_bytes(),
        &bytes,
    ]);
    let so = cache_dir.join(format!("{hash}.so"));
    if so.exists() {
        return Ok(so);
    }

    let tmp = crate::store::staging_path(&so);
    let output = Command::new(cc)
        .args(&link)
        .arg("-o")
        .arg(&tmp)
        .output()
        .map_err(|e| {
            Error::io(
                format!("cannot launch cc to convert `{}`", archive.display()),
                e,
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
                &shape,
                &interpose,
                &native_runtime_bridge,
                linker,
            )?
        {
            return Ok(so);
        }
        return Err(Error::unsupported(format!(
            "Static native archive `{}` cannot be safely converted to a shared library (requires ELF PIC, dependencies closed within this archive):\n{}{}",
            archive.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    crate::store::publish(&so, &tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(
            format!("cannot publish the native archive cache `{}`", so.display()),
            e,
        )
    })?;
    Ok(so)
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
    interpose: &[String],
    native_runtime_bridge: &Path,
    linker: &mut crate::lower::linker::Linker<'_>,
) -> Result<Option<PathBuf>, Error> {
    use rustc_span::Symbol;
    let undefs = crate::native::symtab::archive_undefined_symbols(
        &archive.display().to_string(),
        crate::os::dll::OBJECT_FORMAT,
    )?;
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
        let addr = linker.fn_entry_addr(inst).map_err(|e| {
            Error::malformed(format!(
                "Budgeting P1 entry for rlib symbol `{name}` failed: {e}"
            ))
        })?;
        pairs.push((name, addr));
    }
    pairs.sort();
    // Hidden trampoline assembly. The bridge only references hidden data slots; each Engine writes
    // its own P1 closure address into its copy of the .so, so the artifact does not bake in a fixed
    // runtime address.
    let fmt = super::asmtext::Vocabulary::of(crate::os::dll::OBJECT_FORMAT);
    let mut asm = String::from(crate::arch::asm_text::DIRECTIVE_INTEL);
    let mut slots = std::collections::BTreeSet::new();
    for (name, addr) in &pairs {
        let slot = crate::vm::ir::native_entry_slot_name(crate::vm::ir::LinkAddr(*addr));
        fmt.define_fn(&mut asm, name, super::asmtext::Visibility::Private);
        asm.push_str(&crate::arch::asmstub::indirect_jump_asm(&fmt.symbol(&slot)));
        asm.push('\n');
        slots.insert(slot);
    }
    if !slots.is_empty() {
        fmt.open(&mut asm, super::asmtext::Region::Slots { name: "mirvm_p1" });
        asm.push_str(".balign 8\n");
        for slot in slots {
            fmt.define_slot(&mut asm, &slot, super::asmtext::Visibility::Private);
        }
        fmt.close(&mut asm);
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
    // Assemble trampoline object (same cc path as the archive conversion)
    let s_path = cache_dir.join(format!("{hash}.s"));
    std::fs::write(&s_path, &asm).map_err(|e| {
        Error::io(
            format!("cannot write the rlib trampoline assembly `{s_path:?}`"),
            e,
        )
    })?;
    let o_path = cache_dir.join(format!("{hash}.tramp.o"));
    let st = Command::new(cc)
        .arg("-c")
        .arg("-o")
        .arg(&o_path)
        .arg(&s_path)
        .status()
        .map_err(|e| {
            Error::io(
                "cannot launch cc to assemble the rlib trampoline (is cc missing from PATH?)",
                e,
            )
        })?;
    if !st.success() {
        return Err(Error::tool(format!(
            "cc assembly of rlib trampoline failed (status={st})"
        )));
    }
    // Relink: trampoline object placed after archive so its defined symbols bind unresolved references inside the archive
    let tmp = crate::store::staging_path(&so);
    let output = Command::new(cc)
        .args(native_archive_link(
            archive,
            &[&o_path],
            native_runtime_bridge,
            interpose,
            extra_flags,
        ))
        .arg("-o")
        .arg(&tmp)
        .output()
        .map_err(|e| {
            Error::io(
                format!("cannot launch cc for the relink of `{}`", archive.display()),
                e,
            )
        })?;
    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(None);
    }
    crate::store::publish(&so, &tmp).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(
            format!("cannot publish the native archive cache `{}`", so.display()),
            e,
        )
    })?;
    Ok(Some(so))
}

/// The link line every image this module produces is built from.
///
/// The order is load-bearing twice over. Every member of `archive` goes in, because the symbols
/// mirvm resolves out of it are reached by name at run time. And the bridge comes after the
/// archive, so the definitions it carries win over the host libraries' -- on a platform that binds
/// a call to the first library defining it, that position *is* the redirection.
///
/// `extra_objects` are the objects this module generates for a link that needs them, placed after
/// the archive so their definitions bind references the archive left unresolved.
fn native_archive_link(
    archive: &Path,
    extra_objects: &[&Path],
    bridge: &Path,
    interpose: &[String],
    extra_flags: &[String],
) -> Vec<std::ffi::OsString> {
    let mut line: Vec<std::ffi::OsString> = crate::os::linker::NATIVE_ARCHIVE_PREFIX
        .iter()
        .map(std::ffi::OsString::from)
        .collect();
    line.extend(crate::os::linker::whole_archive(archive));
    line.extend(extra_objects.iter().map(|object| (*object).into()));
    line.push(bridge.into());
    line.extend(interpose.iter().map(std::ffi::OsString::from));
    line.extend(
        crate::os::linker::NATIVE_ARCHIVE_SUFFIX
            .iter()
            .map(std::ffi::OsString::from),
    );
    line.extend(extra_flags.iter().map(std::ffi::OsString::from));
    line
}

fn compiler_identity(cc: &Path) -> Result<Vec<u8>, Error> {
    let version = Command::new(cc).arg("--version").output().map_err(|e| {
        Error::io(
            format!(
                "cannot run the C compiler `{}` for its version",
                cc.display()
            ),
            e,
        )
    })?;
    if !version.status.success() {
        return Err(Error::tool(format!(
            "Querying C compiler `{}` version failed: {}",
            cc.display(),
            String::from_utf8_lossy(&version.stderr)
        )));
    }
    let machine = Command::new(cc).arg("-dumpmachine").output().map_err(|e| {
        Error::io(
            format!(
                "cannot run the C compiler `{}` for its target",
                cc.display()
            ),
            e,
        )
    })?;
    if !machine.status.success() {
        return Err(Error::tool(format!(
            "Querying C compiler `{}` target failed: {}",
            cc.display(),
            String::from_utf8_lossy(&machine.stderr)
        )));
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

/// Whether `name` is one of the sections the old gcc trick put bare function bodies in.
fn is_legacy_init(name: &str) -> bool {
    let numbered = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| rest.chars().next().is_some_and(|c| c.is_ascii_digit()))
    };
    name == ".init" || name == ".fini" || numbered(".init.") || numbered(".fini.")
}

/// Reject a static archive whose members carry the legacy `.init`/`.fini` sections.
///
/// The sections are read here rather than by a tool, because the tool's name, its flags and the
/// spelling it prints a section under are each the object format's, and this build meets two of
/// those. Only one of the two has the sections at all: `.init`/`.fini` are an ELF practice, and a
/// Mach-O object's constructors are the pointer arrays `crate::native::lifecycle` reads and
/// suppresses, so there is nothing of this kind to reject there.
fn reject_legacy_init_sections(archive: &Path) -> Result<(), Error> {
    if crate::os::dll::OBJECT_FORMAT != crate::os::dll::ObjectFormat::Elf {
        return Ok(());
    }
    let bytes = std::fs::read(archive).map_err(|e| {
        Error::io(
            format!(
                "cannot read the static native archive `{}`",
                archive.display()
            ),
            e,
        )
    })?;
    let members = crate::native::ar::members(&bytes).map_err(|error| {
        Error::malformed(format!(
            "cannot read the members of static native archive `{}`: {error:?}",
            archive.display()
        ))
    })?;
    for member in members {
        let Some(header) = crate::native::elf::FileHeader::parse(member) else {
            // The archive's own symbol index is not an object; a member that is not ELF has no
            // section of this kind either.
            continue;
        };
        let Some(names) = crate::native::elf::section_names(member, &header) else {
            return Err(Error::malformed(format!(
                "Cannot read the section names of a member of static native archive `{}`",
                archive.display()
            )));
        };
        if let Some(name) = names.iter().find(|name| is_legacy_init(name)) {
            return Err(Error::unsupported(format!(
                "Rejecting static native archive `{}` with a legacy `{name}` section: \
                 the old gcc trick of injecting bare function bodies into the `.init`/`.fini` \
                 frame has unreliable execution semantics (.init_array family is allowed)",
                archive.display()
            )));
        }
    }
    Ok(())
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
