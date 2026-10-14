//! The base-image cache: the pre-lowered std layer, shared across programs.
//!
//! A miss is built from canonical std roots in the program's compiler session, then retained in
//! memory while the program delta is lowered. The isolated builder uses the same roots.
//! The file records build/sysroot/profile identity, postcard metadata, aligned function chunks and
//! a checksum. Layer keys also include the content digest: an absolute-id consumer cannot bind to
//! a different closure even when its compiler settings match.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::entry;
use crate::vm::archive::{bytes::Bytes, functions};
use crate::vm::ir;

/// Postcard metadata followed by an aligned immutable function archive and a BLAKE3 checksum.
///
/// **Byte-determinism contract** (two consecutive builds must compare equal): `Module`'s
/// exports and function-entry links are HashMaps/Vecs built from unordered lowering state, so they
/// cannot be written to disk inside the module -- they are extracted into **sorted Vec** fields (and
/// cleared inside the module) and rebuilt by the loader. Functions live in the archive, with the
/// module carrying only their names. The remaining fields are determined by lowering order.
#[derive(Serialize, Deserialize)]
struct BaseFile<M = ir::Module> {
    build_id: String,
    /// sysroot freshness (same stamp source as the sysroot build)
    sysroot_stamp: String,
    /// (ub_checks, overflow_checks, contract_checks): the only session booleans lower bakes in
    lowering_fp: (bool, bool, bool),
    profile: Vec<String>,
    /// exports/function-entry links are cleared (see above); rebuilt from the sorted tables below
    module: M,
    /// sym -> FuncId (the sorted form of module.exports)
    export_syms: Vec<(Box<str>, ir::FuncId)>,
    /// LinkAddr -> FuncId (the sorted form of module.fn_entry_links). Sorted by the address: the
    /// keys are unique, so this is a total order and the table is reproducible.
    fn_entry_links: Vec<(ir::LinkAddr, ir::FuncId)>,
    /// sym -> fn entry real address (only functions whose address was taken have an entry)
    fn_entry_syms: Vec<(Box<str>, u64)>,
    static_syms: Vec<(Box<str>, u64)>,
    tls_syms: Vec<(Box<str>, ir::TlsId)>,
}

fn disabled() -> bool {
    crate::options::no_base_image()
}

/// Preserve semantic compiler flags in order. Source/crate names, dependency paths and native
/// inputs do not define the std closure; codegen and unstable flags can affect MIR, layouts and
/// embedded source locations. Only flags known to affect neither are omitted.
fn profile(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "-O" {
            out.push("-Copt-level=2".to_string());
            continue;
        }
        for flag in ["-C", "-Z", "--target", "--cfg", "--remap-path-prefix"] {
            let value = if arg == flag {
                args.next().map(String::as_str)
            } else if flag.len() == 2 {
                arg.strip_prefix(flag)
            } else {
                arg.strip_prefix(flag)
                    .and_then(|rest| rest.strip_prefix('='))
            };
            if let Some(value) = value {
                let name = value.split('=').next().unwrap_or(value);
                if !matches!(
                    (flag, name),
                    (
                        "-C",
                        "metadata"
                            | "extra-filename"
                            | "incremental"
                            | "embed-bitcode"
                            | "debuginfo"
                            | "strip"
                            | "codegen-units"
                    ) | ("-Z", "threads" | "no-codegen" | "mir-opt-level")
                ) {
                    out.push(format!("{flag}{value}"));
                }
                break;
            }
        }
    }
    // rustc remaps imported filenames only when translating virtual rust-src paths back into
    // either sysroot. Other imported paths keep their producer's spelling. A package-local remap
    // therefore cannot change this closure; retain all remaps for the virtual-path test override.
    if !out
        .iter()
        .any(|flag| flag.starts_with("-Zsimulate-remapped-rust-src-base"))
    {
        out.retain(|flag| {
            flag.strip_prefix("--remap-path-prefix")
                .is_none_or(remaps_sysroot)
        });
    }
    // Resolve default check settings so a script and Cargo's explicit dev defaults share std.
    // MIR optimization changes the producer's representation, not its check semantics. The content
    // digest still distinguishes different products when a base is rebuilt by another producer.
    let optimized = out
        .iter()
        .rev()
        .find_map(|flag| flag.strip_prefix("-Copt-level="))
        .is_some_and(|level| level != "0");
    let debug = take_check(&mut out, "-Cdebug-assertions").unwrap_or(!optimized);
    let overflow = take_check(&mut out, "-Coverflow-checks").unwrap_or(debug);
    let ub = take_check(&mut out, "-Zub-checks").unwrap_or(debug);
    let contract = take_check(&mut out, "-Zcontract-checks").unwrap_or(false);
    out.extend([
        format!("debug={debug}"),
        format!("overflow={overflow}"),
        format!("ub={ub}"),
        format!("contract={contract}"),
    ]);
    out
}

fn remaps_sysroot(mapping: &str) -> bool {
    let Some((from, to)) = mapping.rsplit_once('=') else {
        return true;
    };
    let from = Path::new(from);
    if !from.is_absolute() || to.starts_with("/rustc/") || to.starts_with("/rustc-dev/") {
        return true;
    }
    [
        crate::sysroot::toolchain::root(),
        &crate::sysroot::effective_dir(),
    ]
    .iter()
    .any(|root| root.starts_with(from) || from.starts_with(root))
}

fn take_check(flags: &mut Vec<String>, name: &str) -> Option<bool> {
    let mut setting = None;
    flags.retain(|flag| {
        let Some(value) = flag.strip_prefix(name).and_then(|s| {
            if s.is_empty() {
                Some("yes")
            } else {
                s.strip_prefix('=')
            }
        }) else {
            return true;
        };
        let parsed = match value {
            "y" | "yes" | "on" | "true" | "1" => true,
            "n" | "no" | "off" | "false" | "0" => false,
            _ => return true,
        };
        setting = Some(parsed);
        false
    });
    setting
}

fn locate(args: &[String]) -> Option<(PathBuf, String, Vec<String>)> {
    let stamp = crate::sysroot::current_stamp_value()?;
    let profile = profile(args);
    let mut key = entry::Key::new();
    key.part(&stamp);
    for flag in &profile {
        key.part(flag);
    }
    Some((
        crate::store::BASE
            .dir()
            .join(format!("{}.img", key.digest())),
        stamp,
        profile,
    ))
}

fn layer_key(stamp: &str, fp: (bool, bool, bool), digest: blake3::Hash) -> String {
    let mut key = entry::Key::new();
    key.part(stamp)
        .part(&format!("fp{}{}{}", fp.0 as u8, fp.1 as u8, fp.2 as u8))
        .part(&digest.to_hex());
    key.as_str().to_string()
}

fn load(path: &Path, want_stamp: &str, profile: &[String]) -> Option<crate::image::BaseImage> {
    load_bytes(std::fs::read(path).ok()?, want_stamp, profile)
}

fn load_bytes(
    data: Vec<u8>,
    want_stamp: &str,
    profile: &[String],
) -> Option<crate::image::BaseImage> {
    let bytes = Bytes::new(data);
    let data_len = bytes.len().checked_sub(32)?;
    let (data, hash) = bytes.split_at(data_len);
    let digest = blake3::hash(data);
    if digest.as_bytes().as_slice() != hash {
        return None;
    }
    let (f, remaining) = postcard::take_from_bytes::<BaseFile>(data).ok()?;
    if !entry::is_current_generation(&f.build_id)
        || f.sysroot_stamp != want_stamp
        || f.profile != profile
    {
        return None;
    }
    // Check identity and the recorded domain before attempting to restore the mapping.
    if !entry::frozen_at(
        &f.module,
        Some(crate::os_arch::addrspace::BASE_IMAGE_FIXED_ADDR),
    ) || !f.module.funcs.is_empty()
    {
        return None;
    }
    let start =
        (data_len - remaining.len()).checked_next_multiple_of(rkyv::AlignedVec::ALIGNMENT)?;
    let functions = functions::Functions::load(bytes, start..data_len).ok()?;
    if f.module.function_names.len() != functions.len()
        || f.module
            .function_names
            .iter()
            .zip(functions.bodies())
            .any(|(name, body)| name.as_ref() != body.name.as_ref())
    {
        return None;
    }
    let fp = f.lowering_fp;
    let key = layer_key(want_stamp, fp, digest);
    // Rebuild the exports and the function-entry link table from the sorted tables (byte-determinism
    // contract, see BaseFile)
    let mut module = f.module;
    module.funcs = ir::FuncTable::from_archive(std::sync::Arc::new(functions));
    module.exports = f.export_syms.iter().cloned().collect();
    module.fn_entry_links = f.fn_entry_links;
    let instance = entry::revive(&mut module, crate::vm::verify::Below::default())?;
    Some(crate::image::BaseImage {
        fn_by_sym: f.export_syms.into_iter().collect(),
        entry_by_sym: f.fn_entry_syms.into_iter().collect(),
        static_by_sym: f.static_syms.into_iter().collect(),
        tls_by_sym: f.tls_syms.into_iter().collect(),
        lowering_fp: fp,
        key,
        module,
        instance,
        unit: None,
    })
}

/// Read only: a miss is built by `build_in_session` after the user's frontend succeeds.
fn lookup(args: &[String], timing: &mut Timing) -> Option<crate::image::BaseImage> {
    if disabled() {
        return None;
    }
    let (path, stamp, profile) = locate(args)?;
    let started = std::time::Instant::now();
    let loaded = load(&path, &stamp, &profile);
    timing.load += started.elapsed();
    loaded
}

/// Reuse the freshly lowered mapping and archive without another compiler or disk read. Publication
/// is best effort: a complete in-memory layer can still execute when the cache directory is unwritable.
#[cfg(feature = "compiler")]
pub(crate) fn build_in_session(
    tcx: rustc_middle::ty::TyCtxt<'_>,
    args: &[String],
    fp: (bool, bool, bool),
    timing: &mut Timing,
) -> Option<crate::image::BaseImage> {
    if disabled() {
        return None;
    }
    let (path, stamp, profile) = locate(args)?;
    let started = std::time::Instant::now();
    let result = (|| {
        let (mut module, instance, exports) = crate::lower::lower_for_base_build(tcx);
        let (bytes, start) =
            encode(&mut module, &instance, &exports, fp, stamp.clone(), profile).ok()?;
        let end = bytes.len() - 32;
        let digest = blake3::Hash::from_bytes(bytes[end..].try_into().expect("checksum"));
        let bytes = Bytes::new(bytes);
        let functions = functions::Functions::load(bytes.clone(), start..end).ok()?;
        module.funcs = ir::FuncTable::from_archive(std::sync::Arc::new(functions));
        crate::vm::verify::module(&module, &instance).ok()?;
        let _ = std::fs::create_dir_all(crate::store::BASE.dir());
        let _ = crate::store::publish_bytes(&path, &bytes);
        Some(crate::image::BaseImage {
            fn_by_sym: module.exports.clone(),
            entry_by_sym: exports.fn_entry_syms.into_iter().collect(),
            static_by_sym: exports.static_syms.into_iter().collect(),
            tls_by_sym: exports.tls_syms.into_iter().collect(),
            lowering_fp: fp,
            key: layer_key(&stamp, fp, digest),
            module,
            instance,
            unit: None,
        })
    })();
    timing.build = Some(started.elapsed());
    result
}

#[derive(Default)]
pub(crate) struct Timing {
    pub load: std::time::Duration,
    pub build: Option<std::time::Duration>,
}

/// Load the image stack: the base image plus its dependency chain. Only the base image is
/// loaded here.
pub fn ensure(args: &[String], timing: &mut Timing) -> crate::image::ImageStack {
    let mut images = Vec::new();
    if let Some(base) = lookup(args, timing) {
        images.push(base);
    }
    crate::image::ImageStack::from_images(images)
}

/// Serialize and publish one base image: the publishability rules, the byte-determinism extraction
/// (HashMaps into sorted Vecs, the derived address table dropped) and the store write. Called by the
/// isolated builder with a fresh lowering product; the error text is what the caller reports.
#[cfg(feature = "compiler")]
pub(crate) fn store(
    out: &Path,
    mut module: ir::Module,
    instance: &crate::vm::instance::Instance,
    exports: crate::lower::BaseExports,
    fp: (bool, bool, bool),
    args: &[String],
) -> Result<(), crate::image::Error> {
    let sysroot_stamp =
        crate::sysroot::current_stamp_value().ok_or_else(|| crate::image::Error::Unavailable {
            detail: "sysroot stamp unavailable".to_string(),
        })?;
    let (bytes, _) = encode(
        &mut module,
        instance,
        &exports,
        fp,
        sysroot_stamp,
        profile(args),
    )?;
    crate::store::publish_bytes(out, &bytes).map_err(|source| crate::image::Error::Io {
        detail: "cannot write the base image".to_string(),
        source,
    })
}

#[cfg(feature = "compiler")]
fn encode(
    module: &mut ir::Module,
    instance: &crate::vm::instance::Instance,
    exports: &crate::lower::BaseExports,
    fp: (bool, bool, bool),
    sysroot_stamp: String,
    profile: Vec<String>,
) -> Result<(Vec<u8>, usize), crate::image::Error> {
    // A base image is shared across programs, so the fixed-domain contract is stricter than for a
    // delta: the layer above expects the base exactly where it says it is.
    if !entry::snapshot_is_publishable(
        module,
        instance,
        Some(crate::os_arch::addrspace::BASE_IMAGE_FIXED_ADDR),
    ) {
        return Err(crate::image::Error::Contract {
            detail: "frozen region is not in the base-image fixed domain".to_string(),
        });
    }
    // "@entry" is the program-entry alias for --vm-stats; the base image is used as a library, so it
    // does not export the synthetic entry
    module.exports.remove("@entry");
    let funcs = std::mem::take(&mut module.funcs);
    // Byte determinism: extract the unordered tables into sorted Vecs for disk.
    let saved_exports = std::mem::take(&mut module.exports);
    let mut export_syms: Vec<(Box<str>, ir::FuncId)> = saved_exports
        .iter()
        .map(|(s, id)| (s.clone(), *id))
        .collect();
    export_syms.sort_unstable();
    let mut fn_entry_links: Vec<(ir::LinkAddr, ir::FuncId)> =
        std::mem::take(&mut module.fn_entry_links);
    fn_entry_links.sort_unstable_by_key(|(addr, _)| addr.0);
    let saved_links = fn_entry_links.clone();
    let mut fn_entry_syms = exports.fn_entry_syms.clone();
    fn_entry_syms.sort_unstable();
    let mut static_syms = exports.static_syms.clone();
    static_syms.sort_unstable();
    let mut tls_syms = exports.tls_syms.clone();
    tls_syms.sort_unstable();
    let result = (|| {
        let file = BaseFile {
            build_id: crate::options::build::BUILD_ID.to_string(),
            sysroot_stamp,
            lowering_fp: fp,
            profile,
            module: &*module,
            export_syms,
            fn_entry_links,
            fn_entry_syms,
            static_syms,
            tls_syms,
        };
        let mut bytes =
            postcard::to_stdvec(&file).map_err(|error| crate::image::Error::Encode {
                detail: error.to_string(),
            })?;
        let functions = functions::encode(&funcs).map_err(|error| crate::image::Error::Encode {
            detail: error.to_string(),
        })?;
        bytes.resize(bytes.len().next_multiple_of(rkyv::AlignedVec::ALIGNMENT), 0);
        let start = bytes.len();
        bytes.extend_from_slice(&functions);
        let hash = blake3::hash(&bytes);
        bytes.extend_from_slice(hash.as_bytes());
        Ok((bytes, start))
    })();
    module.funcs = funcs;
    module.exports = saved_exports;
    module.fn_entry_links = saved_links;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::instance::Instance;
    use crate::vm::ir::*;

    fn encoded(term: Terminator, reverse: bool) -> Vec<u8> {
        let mut module = Module {
            frozen: Some(
                crate::vm::frozen::FrozenSnapshot::from_home_and_bytes(
                    crate::os_arch::addrspace::BASE_IMAGE_FIXED_ADDR,
                    vec![0; 8],
                )
                .unwrap(),
            ),
            funcs: vec![FuncBody {
                shared: None,
                name: "std_body".into(),
                frame_size: 0,
                frame_align: 1,
                ret: RetAbi::Zst,
                params: Vec::new(),
                caller_loc_off: None,
                blocks: vec![Block {
                    stmts: Vec::new(),
                    term,
                }],
            }]
            .into(),
            ..Default::default()
        };
        for name in if reverse { ["z", "a"] } else { ["a", "z"] } {
            module.exports.insert(name.into(), 0);
        }
        module.ensure_function_names();
        encode(
            &mut module,
            &Instance::default(),
            &crate::lower::BaseExports {
                fn_entry_syms: Vec::new(),
                static_syms: Vec::new(),
                tls_syms: Vec::new(),
            },
            (true, true, false),
            "sysroot".into(),
            Vec::new(),
        )
        .unwrap()
        .0
    }

    #[test]
    fn archive_base_checks_identity_integrity_and_bodies_before_reuse() {
        let bytes = encoded(Terminator::Return, false);
        assert_eq!(bytes, encoded(Terminator::Return, true));
        assert!(load_bytes(bytes.clone(), "different sysroot", &[]).is_none());
        assert!(load_bytes(bytes.clone(), "sysroot", &["-Coverflow-checks=no".into()]).is_none());
        let mut corrupt = bytes.clone();
        let at = corrupt.len() - 33;
        corrupt[at] ^= 1;
        assert!(load_bytes(corrupt, "sysroot", &[]).is_none());
        assert!(load_bytes(encoded(Terminator::Goto(99), false), "sysroot", &[]).is_none());
        let base = load_bytes(bytes.clone(), "sysroot", &[]).unwrap();
        assert_eq!(base.module.funcs.decoded_count(), 0);
        assert!(
            load_bytes(bytes, "sysroot", &[]).is_none(),
            "occupied fixed domain must miss"
        );
        let stack = crate::image::ImageStack::from_images(vec![base]);
        assert!(stack.fp_matches((true, true, false)));
        assert!(!stack.fp_matches((false, false, false)));
        let mut module = Module::default();
        let mut instance = Instance::default();
        stack.absorb_into(&mut module, &mut instance);
        crate::vm::verify::module(&module, &instance).unwrap();
        assert_eq!(module.funcs.decoded_count(), 0);
        assert_eq!(&*module.funcs[0].name, "std_body");
    }

    #[test]
    fn base_profile_preserves_semantic_flags_and_their_order() {
        let args = |values: &[&str]| values.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let first = args(&[
            "mirvm",
            "a.rs",
            "--crate-name=a",
            "--extern=dep=a.rlib",
            "-C",
            "metadata=a",
            "-Cdebuginfo=2",
            "-Zthreads=2",
            "-luser",
            "-C",
            "overflow-checks=no",
            "-Zmir-opt-level=2",
            "--cfg",
            "feature=\"one\"",
            "--target=target",
            "--remap-path-prefix=/old=/new",
        ]);
        let second = args(&[
            "mirvm",
            "b.rs",
            "--crate-name=b",
            "--extern=dep=b.rlib",
            "-Coverflow-checks=no",
            "-Z",
            "mir-opt-level=2",
            "--cfg=feature=\"one\"",
            "--target",
            "target",
            "--remap-path-prefix",
            "/old=/new",
        ]);
        assert_eq!(profile(&first), profile(&second));
        for flag in [
            "-Coverflow-checks=yes",
            "-Ctarget-feature=+avx2",
            "-Zrandomize-layout",
            "-Cpanic=abort",
            "--cfg=other",
            "--remap-path-prefix=/=/last",
        ] {
            let mut changed = second.clone();
            changed.push(flag.into());
            assert_ne!(profile(&second), profile(&changed), "{flag}");
        }
        assert_eq!(
            profile(&args(&["mirvm", "script.rs", "-Zmir-opt-level=2"])),
            profile(&args(&[
                "mirvm",
                "cargo.rs",
                "-Cdebug-assertions=yes",
                "-Coverflow-checks=on"
            ]))
        );
        assert_ne!(profile(&args(&["mirvm", "-O"])), profile(&args(&["mirvm"])));
        assert_eq!(
            profile(&args(&["mirvm", "-O"])),
            profile(&args(&["mirvm", "-Copt-level=2"]))
        );
        assert_ne!(
            profile(&args(&[
                "mirvm",
                "-Coverflow-checks=no",
                "-Coverflow-checks=yes"
            ])),
            profile(&args(&[
                "mirvm",
                "-Coverflow-checks=yes",
                "-Coverflow-checks=no"
            ]))
        );
    }

    #[test]
    fn base_layer_identity_includes_the_actual_closure() {
        let first = blake3::hash(&encoded(Terminator::Return, false));
        let second = blake3::hash(&encoded(Terminator::Goto(0), false));
        assert_ne!(
            layer_key("sysroot", (true, true, false), first),
            layer_key("sysroot", (true, true, false), second)
        );
    }
}
