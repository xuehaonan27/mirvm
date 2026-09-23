//! Writing a package from the clean snapshot right after lowering, the same point as the L2 store:
//! everything is validated and formatted before the filesystem is touched, and the release is atomic.

use std::path::Path;

use super::Error;
use super::format::{
    TAG_FUNCS, TAG_MC, TAG_META, TAG_MODULE, TAG_NATIVELIBS, TAG_RELOC, TAG_STAMPS,
    build_container, hash128,
};
use super::funcs::build_function_section;
use super::meta::{
    McEntry, Meta, ModuleMetaRef, NativeLibEntry, Reloc, host_target, postcard_bytes,
};

/// Rejection is a plain reason string (missing fixed base, unreadable native library); the caller
/// aborts loudly.
pub(crate) fn write_package(
    tcx: rustc_middle::ty::TyCtxt<'_>,
    rustc_args: &[String],
    module: &crate::vm::ir::Module,
    instance: &crate::vm::instance::Instance,
    out: &Path,
) -> Result<(), Error> {
    crate::vm::verify::module(module, instance)
        .map_err(|e| Error::reject(format!("refusing to package invalid bytecode: {e}")))?;
    // Fixed-base requirement (same contract as L2): without a fixed base the snapshot's embedded
    // addresses are invalid across processes, so no package is produced.
    if !instance.frozen.as_ref().is_some_and(|f| f.at_fixed_base()) {
        return Err(Error::reject(
            "frozen region is not at a fixed base (concurrent claim/ASLR conflict); retry packing",
        ));
    }
    if !module.entry_stub_sites.is_empty() && !instance.entry_stubs.at_fixed_base() {
        return Err(Error::reject(
            "entry stub region is not at a fixed base; retry packing",
        ));
    }
    // Input stamps and the env! list are provenance only. Executable semantics are already frozen
    // into the Module, so running a distributed package must not require the source at its original
    // path, nor a replica of the build environment on the target machine.
    let inputs = crate::depinfo::InputManifest::collect(tcx).unwrap_or_default();
    let meta = Meta {
        args: rustc_args.to_vec(),
        envs: inputs.envs,
        base_key: None,
        target: host_target(),
    };
    let reloc = Reloc {
        requires_fixed_base: false,
        entry: "main".into(),
    };
    // NATIVELIBS: every produced library's bytes travel with the package. The global_asm family
    // also enters the MC section for in-process loading; MIRVM_PACK_NO_MC=1 only switches the load
    // method and no longer breaks the package's self-containment.
    let ga_prefix = crate::store::GLOBAL_ASM.dir().display().to_string();
    let no_mc = crate::options::get().pack_no_mc;
    let mut libs = Vec::new();
    let mut mc_entries = Vec::new();
    for p in &module.required_native_libs {
        let data = std::fs::read(&**p)
            .map_err(|e| Error::io(format!("cannot read the produced library `{p}`"), e))?;
        let fnv = hash128(&data);
        let role = u8::from(p.starts_with(&ga_prefix));
        if role == 1 && !no_mc && !mc_entries.iter().any(|m: &McEntry| m.fnv == fnv) {
            mc_entries.push(McEntry {
                fnv,
                bytes: data.clone(),
            });
        }
        libs.push(NativeLibEntry {
            path: p.to_string(),
            role,
            fnv,
            bytes: data,
        });
    }
    let module_bytes = postcard_bytes(&ModuleMetaRef::from((module, instance)))
        .map_err(|e| Error::build(format!("cannot serialize module metadata: {e}")))?;
    let function_bytes = build_function_section(&module.funcs)?;

    let mut sections: Vec<(u32, Vec<u8>)> = vec![
        (TAG_META, postcard_bytes(&meta)?),
        (TAG_STAMPS, postcard_bytes(&inputs.files)?),
        (TAG_MODULE, module_bytes),
        (TAG_NATIVELIBS, postcard_bytes(&libs)?),
        (TAG_RELOC, postcard_bytes(&reloc)?),
        (TAG_FUNCS, function_bytes),
    ];
    if !mc_entries.is_empty() {
        sections.push((TAG_MC, postcard_bytes(&mc_entries)?));
    }

    let buf = build_container(&sections)?;

    // Atomic publish: a reader sees either the previous package or this one, never a half-written
    // file. The output path is the user's, so the staging file lands next to it.
    let dir = out.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::io("cannot create the package directory", e))?;
    crate::store::publish_bytes(out, &buf)
        .map_err(|e| Error::io("cannot release the package", e))?;
    Ok(())
}
