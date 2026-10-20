//! A disposable, already-bound program image. It is self-contained with respect to shared packs:
//! a hit needs input validation and instantiation, but no layer lookup or symbolic rebinding.

use serde::{Deserialize, Serialize};

use crate::depinfo::InputManifest;
use crate::utils::content::FileStamp;
use crate::vm::archive::{Image, VerifiedArtifact, bytes::Bytes, functions, meta};
#[cfg(any(feature = "compiler", test))]
use crate::vm::{instance::Instance, ir::Module};

#[cfg(any(feature = "compiler", test))]
mod reachable;

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
    let mut mark = crate::options::timing().then(std::time::Instant::now);
    let mut lap = || {
        mark.as_mut().map_or(0.0, |previous| {
            let now = std::time::Instant::now();
            let millis = now.duration_since(*previous).as_secs_f64() * 1000.0;
            *previous = now;
            millis
        })
    };
    let bytes = Bytes::new(bytes);
    let (data, hash) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
    let data_len = data.len();
    if blake3::hash(data).as_bytes().as_slice() != hash {
        return None;
    }
    let integrity = lap();
    let (header, data) = postcard::take_from_bytes::<Header>(data).ok()?;
    if !crate::store::entry::is_current_generation(&header.build_id)
        || header.target != crate::options::build::HOST
        || header.args != args
        || !header.inputs.is_current()
        || !header.native_files.iter().all(FileStamp::is_current)
    {
        return None;
    }
    let inputs = lap();
    let (meta, remaining) = postcard::take_from_bytes::<meta::ModuleMeta>(data).ok()?;
    let metadata = lap();
    let start =
        (data_len - remaining.len()).checked_next_multiple_of(rkyv::AlignedVec::ALIGNMENT)?;
    let functions = functions::Functions::load(bytes, start..data_len).ok()?;
    let structure = lap();
    let mut artifact = Image::load_once(meta, functions).ok()?;
    let instantiate_verify = lap();
    let addresses =
        crate::native::artifact::assembly::try_materialize(&artifact.module().asm_sites).ok()?;
    artifact.set_asm_addrs(addresses).ok()?;
    let assembly = lap();
    if crate::options::timing() {
        crate::diag::instrument(format_args!(
            "mirvm-image-load: integrity={integrity:.3}ms inputs={inputs:.3}ms metadata={metadata:.3}ms structure={structure:.3}ms instantiate-verify={instantiate_verify:.3}ms assembly={assembly:.3}ms"
        ));
    }
    Some(artifact)
}

#[cfg(feature = "compiler")]
pub(crate) fn store(
    args: &[String],
    inputs: InputManifest,
    module: &Module,
    instance: &Instance,
) -> Result<(), crate::error::Error> {
    if disabled() {
        return Err(crate::fail!(Image, "execution-image caching is disabled"));
    }
    let bytes = encode(args, inputs, module, instance)?;
    std::fs::create_dir_all(crate::store::EXECUTION.dir()).map_err(|error| {
        crate::fail!(
            Image,
            format!("cannot create execution-image directory: {error}")
        )
    })?;
    crate::store::publish_bytes(&path(args), &bytes)
        .map_err(|error| crate::fail!(Image, format!("cannot publish execution image: {error}")))
}

#[cfg(any(feature = "compiler", test))]
fn encode(
    args: &[String],
    inputs: InputManifest,
    module: &Module,
    instance: &Instance,
) -> Result<Vec<u8>, crate::error::Error> {
    if !crate::store::entry::snapshot_is_publishable(module, instance, None)
        || instance
            .image_frozens
            .iter()
            .any(|arena| !arena.at_fixed_base())
    {
        return Err(crate::fail!(
            Image,
            "execution image does not occupy its fixed address domains"
        ));
    }
    crate::vm::verify::module(module, instance).map_err(|error| {
        crate::fail!(
            Image,
            format!("execution image failed verification: {error}")
        )
    })?;
    let native_files = module
        .required_native_libs
        .iter()
        .map(|path| {
            FileStamp::of(std::path::Path::new(path.as_ref())).map_err(|error| {
                crate::fail!(Image, format!("cannot stamp native input {path}: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let header = Header {
        build_id: crate::options::build::BUILD_ID.into(),
        target: crate::options::build::HOST.into(),
        args: args.to_vec(),
        inputs,
        native_files,
    };
    let mut bytes = postcard::to_stdvec(&header).map_err(|error| {
        crate::fail!(
            Image,
            format!("cannot encode execution-image header: {error}")
        )
    })?;
    let metadata =
        postcard::to_stdvec(&meta::ModuleMetaRef::from((module, instance))).map_err(|error| {
            crate::fail!(
                Image,
                format!("cannot encode execution-image metadata: {error}")
            )
        })?;
    let functions = functions::encode(&module.funcs).map_err(|error| {
        crate::fail!(
            Image,
            format!("cannot encode execution-image functions: {error}")
        )
    })?;
    if crate::options::timing() {
        let live = reachable::functions(module, instance);
        crate::diag::instrument(format_args!(
            "mirvm-image-store: slots={} reachable={} metadata={}B code={}B",
            live.len(),
            live.iter().filter(|keep| **keep).count(),
            metadata.len(),
            functions.len()
        ));
    }
    bytes.reserve(metadata.len() + rkyv::AlignedVec::ALIGNMENT - 1 + functions.len() + 32);
    bytes.extend(metadata);
    bytes.resize(bytes.len().next_multiple_of(rkyv::AlignedVec::ALIGNMENT), 0);
    bytes.extend_from_slice(&functions);
    let hash = blake3::hash(&bytes);
    bytes.extend_from_slice(hash.as_bytes());
    Ok(bytes)
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
            shared: None,
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
