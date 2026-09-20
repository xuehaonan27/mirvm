//! The base-image cache: the pre-lowered std layer, shared across programs.
//!
//! The layer itself is a synthetic "empty main" session (lang_start chain / panic / fmt / alloc
//! machinery, ~3000 instances) whose frozen region sits in the `BASE_IMAGE_FIXED_ADDR` domain. The
//! stack that consumes it, and the offset merge that folds it under a program delta, are
//! [`crate::image`]; this module is the file: where it lives, when it is valid, and what the
//! build subprocess (`crate::cli::base_image`) writes.
//!
//! Key and invalidation: the file name is `digest(build_id, sysroot stamp)`, and loading re-checks
//! that the build id and the stamp are equal. The **lowering fingerprint** (the three session
//! booleans lower bakes into bytecode: ub/overflow/contract checks) must be verified **within the
//! session** (a cargo runner may pass custom profile flags); on mismatch the base image is dropped
//! and everything is lowered cold (self-heal, silent fallback; same on build failure, with the log
//! in `cache/base/build.log`). `MIRVM_NO_BASE_IMAGE=1` bypasses the base image entirely
//! (differential/diagnostic use).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::entry;
use crate::vm::ir;

/// Base image file (v1: one postcard blob).
///
/// **Byte-determinism contract** (two consecutive builds must compare equal): `Module`'s
/// exports/fn_addrs/link_fn_addrs are std HashMaps, whose RandomState seed makes iteration order
/// per-process random, so they cannot be written to disk inside the module -- they are
/// extracted into **sorted Vec** fields (and cleared inside the module) and rebuilt by the
/// loader. The remaining fields (funcs/tls/asm_sites/frozen region bytes) are determined
/// by lowering order.
#[derive(Serialize, Deserialize)]
struct BaseFile {
    build_id: String,
    /// sysroot freshness (same stamp source as the sysroot build)
    sysroot_stamp: String,
    /// (ub_checks, overflow_checks, contract_checks): the only session booleans lower bakes in
    lowering_fp: (bool, bool, bool),
    /// exports/fn_addrs/link_fn_addrs are cleared and asm_stub_addrs emptied (see above); rebuilt
    /// from the sorted tables below and rematerialized by the loader
    module: ir::Module,
    /// sym -> FuncId (the sorted form of module.exports)
    export_syms: Vec<(Box<str>, ir::FuncId)>,
    /// fn entry real address -> FuncId (the sorted form of module.fn_addrs)
    fn_addr_pairs: Vec<(u64, ir::FuncId)>,
    /// LinkAddr -> FuncId (the sorted form of module.link_fn_addrs). Sorted by the address: the
    /// keys are unique, so this is a total order and the table is reproducible.
    link_fn_addr_pairs: Vec<(ir::LinkAddr, ir::FuncId)>,
    /// sym -> fn entry real address (only functions whose address was taken have an entry)
    fn_entry_syms: Vec<(Box<str>, u64)>,
    static_syms: Vec<(Box<str>, u64)>,
    tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

fn disabled() -> bool {
    crate::options::get().no_base_image
}

fn base_dir() -> PathBuf {
    crate::store::BASE.dir()
}

/// (base image path, sysroot stamp). `None` when the stamp is unavailable (sysroot not
/// built yet, etc.), i.e. no base image.
fn locate() -> Option<(PathBuf, String)> {
    let stamp = crate::sysroot::current_stamp_value()?;
    let mut key = entry::Key::new();
    key.part(&stamp);
    Some((base_dir().join(format!("{}.img", key.digest())), stamp))
}

fn load(path: &Path, want_stamp: &str) -> Option<crate::image::BaseImage> {
    let data = std::fs::read(path).ok()?;
    let f: BaseFile = postcard::from_bytes(&data).ok()?; // restores the frozen region at its fixed base; a taken region means a miss
    if !entry::is_current_generation(&f.build_id) || f.sysroot_stamp != want_stamp {
        return None;
    }
    // The frozen region must really land in the base-image domain (rejects a swapped file
    // or a taken domain alike)
    if !entry::frozen_at(
        &f.module,
        Some(crate::vm::addrlayout::BASE_IMAGE_FIXED_ADDR),
    ) {
        return None;
    }
    let fp = f.lowering_fp;
    // The layer key carries the lowering fingerprint, which the file name does not.
    let mut key = entry::Key::new();
    key.part(want_stamp)
        .part(&format!("fp{}{}{}", fp.0 as u8, fp.1 as u8, fp.2 as u8));
    let key = key.as_str().to_string();
    // Rebuild exports/fn_addrs from the sorted tables (byte-determinism contract, see BaseFile)
    let mut module = f.module;
    module.exports = f.export_syms.iter().cloned().collect();
    module.fn_addrs = f.fn_addr_pairs.iter().copied().collect();
    module.link_fn_addrs = f.link_fn_addr_pairs.iter().copied().collect();
    if !entry::revive(&mut module, crate::vm::verify::Prefix::default()) {
        return None;
    }
    Some(crate::image::BaseImage {
        fn_by_sym: f.export_syms.into_iter().collect(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym: f.tls_syms.into_iter().collect(),
        lowering_fp: fp,
        key,
        module,
    })
}

/// Load the base image, or build it in a subprocess. Failure yields `None` (full cold
/// lowering, silent self-heal); the build log goes to base/build.log because stderr
/// participates in native differential comparison, so the main path must stay quiet.
fn ensure_base() -> Option<crate::image::BaseImage> {
    if disabled() {
        return None;
    }
    let (path, stamp) = locate()?;
    if let Some(base) = load(&path, &stamp) {
        return Some(base);
    }
    // Build in a subprocess: this process's rustc-session uniqueness (TRACK_DIAGNOSTIC and
    // the global counters) forbids a second compiler. The exec itself costs ~13ms and
    // happens once.
    let self_exe = std::env::current_exe().ok()?;
    let _ = std::fs::create_dir_all(base_dir());
    let log = std::fs::File::create(base_dir().join("build.log")).ok()?;
    let status = std::process::Command::new(self_exe)
        .arg("__build-base-image")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    load(&path, &stamp)
}

/// Load the image stack: the base image plus its dependency chain. Only the base image is
/// loaded here.
pub fn ensure() -> crate::image::ImageStack {
    let mut images = Vec::new();
    if let Some(base) = ensure_base() {
        images.push(base);
    }
    crate::image::ImageStack::from_images(images)
}

/// Serialize and publish one base image: the publishability rules, the byte-determinism extraction
/// (HashMaps into sorted Vecs, the derived address table dropped) and the store write. Called by the
/// build subprocess with a fresh lowering product; the error text is what the caller reports.
pub(crate) fn store(
    out: &Path,
    mut module: ir::Module,
    exports: crate::lower::BaseExports,
    fp: (bool, bool, bool),
) -> Result<(), String> {
    // A base image is shared across programs, so the fixed-domain contract is stricter than for a
    // delta: the layer above expects the base exactly where it says it is.
    if !entry::snapshot_is_publishable(&module, Some(crate::vm::addrlayout::BASE_IMAGE_FIXED_ADDR))
    {
        return Err("frozen region is not in the base-image fixed domain".into());
    }
    // "@entry" is the program-entry alias for --vm-stats; the base image is used as a library, so it
    // does not export the synthetic entry
    module.exports.remove("@entry");
    let sysroot_stamp =
        crate::sysroot::current_stamp_value().ok_or("sysroot stamp unavailable".to_string())?;
    // Byte determinism: extract the HashMaps (RandomState gives a random iteration order) into
    // sorted Vecs for disk, and drop the derived address table.
    //
    // `asm_stub_addrs` holds this process's dlopen addresses for the stub .so, so it is stale in a
    // file by construction; every consumer rematerializes it from `asm_sites` (the cold lowering
    // path, `ImageStack::absorb_into`, the L2-hit path, package load).
    module.asm_stub_addrs.clear();
    let mut export_syms: Vec<(Box<str>, ir::FuncId)> = module.exports.drain().collect();
    export_syms.sort_unstable();
    let mut fn_addr_pairs: Vec<(u64, ir::FuncId)> = module.fn_addrs.drain().collect();
    fn_addr_pairs.sort_unstable();
    let mut link_fn_addr_pairs: Vec<(ir::LinkAddr, ir::FuncId)> =
        module.link_fn_addrs.drain().collect();
    link_fn_addr_pairs.sort_unstable_by_key(|(addr, _)| addr.0);
    let mut fn_entry_syms = exports.fn_entry_syms;
    fn_entry_syms.sort_unstable();
    let mut static_syms = exports.static_syms;
    static_syms.sort_unstable();
    let mut tls_syms = exports.tls_syms;
    tls_syms.sort_unstable();
    let file = BaseFile {
        build_id: crate::options::build::BUILD_ID.to_string(),
        sysroot_stamp,
        lowering_fp: fp,
        module,
        export_syms,
        fn_addr_pairs,
        link_fn_addr_pairs,
        fn_entry_syms,
        static_syms,
        tls_syms,
    };
    let bytes =
        postcard::to_stdvec(&file).map_err(|error| format!("serialization failed: {error}"))?;
    crate::store::publish_bytes(out, &bytes)
        .map_err(|error| format!("writing the file failed: {error}"))
}
