//! Compiler metadata discovery and Rust symbol recovery for native archive links.

use super::*;
use rustc_hir::attrs::NativeLibKind;
use rustc_hir::def_id::LOCAL_CRATE;
use rustc_middle::ty::TyCtxt;
use rustc_session::search_paths::PathKind;
use rustc_target::spec::{BinaryFormat, Os};

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
fn system_dylibs(tcx: TyCtxt<'_>) -> Vec<Box<str>> {
    system_dylibs_for(tcx, None)
}

/// Limit a shared base to sysroot metadata; CLI libraries belong to the program.
pub(crate) fn system_dylibs_for(
    tcx: TyCtxt<'_>,
    crates: Option<&rustc_data_structures::fx::FxHashSet<rustc_hir::def_id::CrateNum>>,
) -> Vec<Box<str>> {
    let sess = tcx.sess;
    let mut names: Vec<Box<str>> = Vec::new();
    for cnum in std::iter::once(LOCAL_CRATE).chain(tcx.used_crates(()).iter().copied()) {
        if crates.is_some_and(|crates| !crates.contains(&cnum))
            || (cnum != LOCAL_CRATE && tcx.crate_dep_kind(cnum).macros_only())
        {
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
    for lib in sess.opts.libs.iter().filter(|_| crates.is_none()) {
        if matches!(lib.kind, NativeLibKind::Static { .. }) {
            continue;
        }
        let name: Box<str> = lib.name.as_str().into();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if crates.is_some() {
        names.sort_unstable();
    }
    names
}

/// Collect the current crate graph's Static native libraries and convert each independent archive
/// into a `.so`.
///
/// Only Linux/ELF is handled. Each archive is linked independently with `-z defs`, so cross-archive
/// dependencies, dependency ordering and non-PIC relocations fail loudly; no generic native link
/// plan is guessed.
/// On conversion failure, fall back to the "symbols in rlib" rescue chain
/// (undefined ∩ crate-graph rlib exported fn => inject hidden P1-entry trampolines and relink;
/// an unavailable rescue retains the original error path).
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
                |failed| rescue_with_rlib_symbols(failed, linker),
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

fn find_archive(filename: &str, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    search_dirs
        .iter()
        .map(|dir| dir.join(filename))
        .find(|path| path.is_file())
}

/// "Symbols in rlib" rescue chain for a first link that failed on undefined symbols:
/// statically enumerate the archive's SHN_UNDEF symbols ∩ crate-graph rlib exported-fn set
/// (`Linker::exported_defs`, the same source as the native final-link symbol set). For each
/// function in the intersection, budget P1 executable entries, emit `.hidden` trampolines, and
/// merge them into the relink. Returns None if the rescue cannot apply (no intersection /
/// non-derivable signature / relink still fails); the caller then takes the original error path.
/// Err is a materialization-time diagnosis (enumeration or trampoline assembly failed), under the
/// same loud-rejection discipline as `-z defs`.
fn rescue_with_rlib_symbols(
    failed: &FailedLink<'_>,
    linker: &mut crate::lower::linker::Linker<'_>,
) -> Result<Option<PathBuf>, Error> {
    let FailedLink {
        archive,
        cache_dir,
        target,
        cc,
        extra_flags,
        archive_bytes,
        cc_identity,
        link_flags,
        interpose,
        native_runtime_bridge,
    } = *failed;
    use rustc_span::Symbol;
    let undefs = crate::native::symbol::symtab::archive_undefined_symbols(
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
    if crate::options::c2_debug() {
        crate::diag::instrument(format_args!(
            "c2-debug: undefs={undefs:?} hit={}",
            hit.len()
        ));
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
    let fmt = crate::native::artifact::asmtext::Vocabulary::of(crate::os::dll::OBJECT_FORMAT);
    let mut asm = String::from(crate::arch::asm_text::DIRECTIVE_INTEL);
    let mut slots = std::collections::BTreeSet::new();
    for (name, addr) in &pairs {
        let slot = crate::vm::ir::native_entry_slot_name(crate::vm::ir::LinkAddr(*addr));
        fmt.define_fn(
            &mut asm,
            name,
            crate::native::artifact::asmtext::Visibility::Private,
        );
        asm.push_str(&crate::os_arch::bridge::slot_jump_asm(&fmt.symbol(&slot)));
        asm.push('\n');
        slots.insert(slot);
    }
    if !slots.is_empty() {
        fmt.open(
            &mut asm,
            crate::native::artifact::asmtext::Region::Slots { name: "mirvm_p1" },
        );
        asm.push_str(".balign 8\n");
        for slot in slots {
            fmt.define_slot(
                &mut asm,
                &slot,
                crate::native::artifact::asmtext::Visibility::Private,
            );
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
