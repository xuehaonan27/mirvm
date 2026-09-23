//! Reading a package: the sniff `run` branches on, and the full refuse-loud load.
//!
//! The load copies the file once and validates exclusively from that immutable snapshot, so its
//! result cannot retain a mutable inode; every function's bytecode is verified before any native
//! image is created.

use std::collections::HashSet;
use std::path::Path;

use serde::de::DeserializeOwned;

use super::Error;
use super::LoadedPackage;
use super::format::{
    MAGIC, ParsedPackage, TAG_BASE, TAG_FUNCS, TAG_MC, TAG_META, TAG_MODULE, TAG_NATIVELIBS,
    TAG_RELOC, TAG_STAMPS, check_hash, hash128, parse_container,
};
use super::funcs::parse_function_section;
use super::meta::{
    McEntry, Meta, ModuleMeta, NativeLibEntry, Reloc, check_lib_hash, host_target,
    native_entry_for_mc,
};

/// The one sentence every bytecode-verification refusal carries; the verifier's own reason travels
/// in the detail.
const MODULE_VERIFY_FAILED: &str = "MODULE bytecode verification failed";

/// Package sniff: the first 8 bytes being the magic marks a package (run's branch criterion).
pub(crate) fn is_package(path: &Path) -> bool {
    let mut b = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| {
            use std::io::Read as _;
            f.read_exact(&mut b)
        })
        .is_ok_and(|()| &b == MAGIC)
}

/// One section, decoded as its own document. `label` is the section name as the message spells it,
/// so a decode failure names the section the reader was working on.
fn section_of<T: DeserializeOwned>(
    package: &ParsedPackage<'_>,
    tag: u32,
    label: &str,
) -> Result<T, Error> {
    postcard::from_bytes(package.section(tag)?)
        .map_err(|e| Error::corrupt(format!("cannot resolve the {label} section: {e}")))
}

/// Load + full verification (refuse-loud). Input stamps and the compile-time environment are
/// provenance only; the Module inside has its semantics frozen, so running it no longer requires the
/// source or the original build environment.
pub(crate) fn load_package(path: &Path) -> Result<LoadedPackage, Error> {
    // `Package::load` is safe, so its result must not retain the filesystem's mutable inode.
    // Copy once, then validate and lazily decode exclusively from this immutable snapshot.
    let raw: std::sync::Arc<[u8]> = std::fs::read(path)
        .map_err(|e| Error::io("cannot read the package", e))?
        .into();
    let package = parse_container(&raw)?;
    let meta: Meta = section_of(&package, TAG_META, "META")?;
    let current_target = host_target();
    if meta.target != current_target {
        return Err(Error::incompatible(format!(
            "package target mismatch (package={}, current={current_target})",
            meta.target
        )));
    }
    if meta.base_key.is_some() || package.has_section(TAG_BASE) {
        return Err(Error::incompatible(
            "package BASE/delta form is not supported by this mirvm",
        ));
    }
    // Stamps are provenance: decoded to prove they survived the round trip, then dropped.
    let _: Vec<crate::utils::content::FileStamp> = section_of(&package, TAG_STAMPS, "STAMPS")?;
    let libs: Vec<NativeLibEntry> = section_of(&package, TAG_NATIVELIBS, "NATIVELIBS")?;
    let mc_entries: Vec<McEntry> = if package.has_section(TAG_MC) {
        section_of(&package, TAG_MC, "MC")?
    } else {
        Vec::new()
    };
    let reloc: Reloc = section_of(&package, TAG_RELOC, "RELOC")?;
    if &*reloc.entry != "main" {
        return Err(Error::reject(format!(
            "unsupported package entry `{}`",
            reloc.entry
        )));
    }
    let module_meta: ModuleMeta = section_of(&package, TAG_MODULE, "MODULE")?;
    let module = module_meta.instantiate()?;
    let function_section = package.section(TAG_FUNCS)?;
    let mapped_offset = function_section.as_ptr() as usize - raw.as_ptr() as usize;
    let function_blobs = parse_function_section(function_section, mapped_offset)?;
    if module.function_names.len() != function_blobs.len() {
        return Err(Error::corrupt(format!(
            "MODULE function name table has {} entries, expected {}",
            module.function_names.len(),
            function_blobs.len()
        )));
    }
    crate::vm::verify::module_header_with_count(&module, function_blobs.len())
        .map_err(|e| Error::reject(format!("{MODULE_VERIFY_FAILED}: {e}")))?;
    // Before any MC/native materialization, every function gets full semantic verification.
    // Temporary objects are dropped each round; the run phase still decodes on demand from the owned
    // snapshot instead of keeping every function resident.
    let mut main_boundaries = 0;
    let mut main_catchers = 0;
    for (index, blob) in function_blobs.iter().enumerate() {
        let body: crate::vm::ir::FuncBody = postcard::from_bytes(&raw[blob.start..blob.end])
            .map_err(|e| {
                Error::corrupt(format!(
                    "function {index} decode failed during verification: {e}"
                ))
            })?;
        crate::vm::verify::function_with_count(&module, function_blobs.len(), index, &body)
            .map_err(|e| Error::reject(format!("{MODULE_VERIFY_FAILED}: {e}")))?;
        let (boundaries, catchers) = crate::vm::verify::body_main_role_counts(&body);
        main_boundaries += boundaries;
        main_catchers += catchers;
    }
    crate::vm::verify::main_role_counts(&module, main_boundaries, main_catchers)
        .map_err(|e| Error::reject(format!("{MODULE_VERIFY_FAILED}: {e}")))?;
    if reloc.requires_fixed_base {
        return Err(Error::reject(
            "package v4 cannot require a fixed runtime base",
        ));
    }
    if module.required_native_libs.len() != libs.len()
        || module
            .required_native_libs
            .iter()
            .zip(&libs)
            .any(|(path, lib)| path.as_ref() != lib.path.as_str())
    {
        return Err(Error::corrupt(
            "package NATIVELIBS does not match MODULE native library order",
        ));
    }
    for lib in &libs {
        if lib.role > 1 {
            return Err(Error::corrupt(format!(
                "package native library `{}` has unknown role {}",
                lib.path, lib.role
            )));
        }
        check_lib_hash(lib)?;
    }

    let mut covered_hashes = HashSet::with_capacity(mc_entries.len());
    for mc in &mc_entries {
        check_hash(&mc.bytes, mc.fnv, "package MC entry has wrong content hash")?;
        if !covered_hashes.insert(mc.fnv) {
            return Err(Error::corrupt(
                "package MC section contains a duplicate image",
            ));
        }
        if native_entry_for_mc(&libs, mc.fnv).is_none() {
            return Err(Error::corrupt(
                "package MC section has no matching NATIVELIBS entry (missing or extra)",
            ));
        }
    }
    let heat_key = format!("{:032x}", hash128(function_section));
    let heat_path = crate::store::PACKAGE_HEAT
        .dir()
        .join(format!("{heat_key}.order"));
    drop(module);
    Ok(LoadedPackage {
        raw,
        module_meta,
        function_blobs,
        libs,
        mc_entries,
        heat_path,
    })
}
