//! A disposable, already-bound program image. It is self-contained with respect to shared packs:
//! a hit needs input validation and instantiation, but no layer lookup or symbolic rebinding.

use serde::{Deserialize, Serialize};

use crate::depinfo::InputManifest;
use crate::utils::content::FileStamp;
use crate::vm::archive::{Image, VerifiedArtifact, bytes::Bytes, functions, meta};
use crate::vm::{instance::Instance, ir::Module};

#[derive(Serialize, Deserialize)]
struct Header {
    build_id: String,
    target: String,
    args: Vec<String>,
    inputs: InputManifest,
    native_files: Vec<FileStamp>,
}

fn disabled() -> bool {
    crate::options::no_ir_cache()
        || crate::options::no_base_image()
        || crate::options::no_deps_image()
}

fn path(args: &[String]) -> std::path::PathBuf {
    crate::store::EXECUTION
        .dir()
        .join(format!("{}.bin", super::program::key_of(args).digest()))
}

pub(crate) fn lookup(args: &[String]) -> Option<VerifiedArtifact> {
    if disabled() {
        return None;
    }
    let bytes = std::fs::read(path(args)).ok()?;
    load_bytes(bytes, args)
}

fn load_bytes(bytes: Vec<u8>, args: &[String]) -> Option<VerifiedArtifact> {
    let bytes = Bytes::new(bytes);
    let (data, hash) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
    let data_len = data.len();
    if blake3::hash(data).as_bytes().as_slice() != hash {
        return None;
    }
    let (header, data) = postcard::take_from_bytes::<Header>(data).ok()?;
    if !crate::store::entry::is_current_generation(&header.build_id)
        || header.target != crate::options::build::HOST
        || header.args != args
        || !header.inputs.is_current()
        || !header.native_files.iter().all(FileStamp::is_current)
    {
        return None;
    }
    let (meta, remaining) = postcard::take_from_bytes::<meta::ModuleMeta>(data).ok()?;
    let start =
        (data_len - remaining.len()).checked_next_multiple_of(rkyv::AlignedVec::ALIGNMENT)?;
    let functions = functions::Functions::load(bytes, start..data_len).ok()?;
    let mut artifact = Image::load_once(meta, functions).ok()?;
    let addresses = crate::lower::asm::try_materialize(&artifact.module().asm_sites).ok()?;
    artifact.set_asm_addrs(addresses).ok()?;
    Some(artifact)
}

pub(crate) fn store(args: &[String], inputs: InputManifest, module: &Module, instance: &Instance) {
    if disabled() {
        return;
    }
    let Some(bytes) = encode(args, inputs, module, instance) else {
        return;
    };
    if std::fs::create_dir_all(crate::store::EXECUTION.dir()).is_err() {
        return;
    }
    let _ = crate::store::publish_bytes(&path(args), &bytes);
}

fn encode(
    args: &[String],
    inputs: InputManifest,
    module: &Module,
    instance: &Instance,
) -> Option<Vec<u8>> {
    if !crate::store::entry::snapshot_is_publishable(module, instance, None)
        || instance
            .image_frozens
            .iter()
            .any(|arena| !arena.at_fixed_base())
        || crate::vm::verify::module(module, instance).is_err()
    {
        return None;
    }
    let native_files = module
        .required_native_libs
        .iter()
        .map(|path| FileStamp::of(std::path::Path::new(path.as_ref())).ok())
        .collect::<Option<Vec<_>>>()?;
    let header = Header {
        build_id: crate::options::build::BUILD_ID.into(),
        target: crate::options::build::HOST.into(),
        args: args.to_vec(),
        inputs,
        native_files,
    };
    let Ok(mut bytes) = postcard::to_stdvec(&header) else {
        return None;
    };
    let Ok(metadata) = postcard::to_stdvec(&meta::ModuleMetaRef::from((module, instance))) else {
        return None;
    };
    let Ok(functions) = functions::encode(&module.funcs) else {
        return None;
    };
    bytes.reserve(metadata.len() + rkyv::AlignedVec::ALIGNMENT - 1 + functions.len() + 32);
    bytes.extend(metadata);
    bytes.resize(bytes.len().next_multiple_of(rkyv::AlignedVec::ALIGNMENT), 0);
    bytes.extend_from_slice(&functions);
    let hash = blake3::hash(&bytes);
    bytes.extend_from_slice(hash.as_bytes());
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::ir::*;

    #[test]
    fn inputs_arguments_and_payload_must_match_without_shared_stores() {
        let dir =
            std::env::temp_dir().join(format!("mirvm-execution-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.rs");
        std::fs::write(&source, b"first").unwrap();
        let inputs = InputManifest {
            files: vec![FileStamp::of(&source).unwrap()],
            ..Default::default()
        };
        let body = FuncBody {
            frame_size: 0,
            frame_align: 1,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            name: "unused".into(),
            blocks: vec![Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            }],
        };
        let mut module = Module {
            funcs: vec![body].into(),
            ..Module::default()
        };
        module.ensure_function_names();
        let frozen = crate::vm::frozen::FrozenArena::new_base_image();
        assert!(
            frozen.at_fixed_base(),
            "test requires an unclaimed base-image domain"
        );
        module.frozen = Some(
            crate::vm::frozen::FrozenSnapshot::from_home_and_bytes(
                frozen.home(),
                frozen.snapshot().to_vec(),
            )
            .unwrap(),
        );
        let instance = Instance {
            frozen: Some(frozen),
            ..Default::default()
        };
        let args = vec!["mirvm".to_string(), source.display().to_string()];
        let bytes = encode(&args, inputs, &module, &instance).unwrap();
        drop(instance);
        let loaded = load_bytes(bytes.clone(), &args).expect("self-contained image should load");
        assert_eq!(loaded.module().funcs.len(), 1);
        assert_eq!(loaded.module().funcs.decoded_count(), 0);
        assert!(load_bytes(bytes.clone(), &["different arguments".into()]).is_none());
        let mut corrupt = bytes.clone();
        let at = corrupt.len() - 33;
        corrupt[at] ^= 1;
        assert!(load_bytes(corrupt, &args).is_none());
        let mtime = std::fs::metadata(&source).unwrap().modified().unwrap();
        std::fs::write(&source, b"other").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        assert!(load_bytes(bytes.clone(), &args).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
