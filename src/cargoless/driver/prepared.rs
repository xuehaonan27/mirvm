//! Validated launch recipes for the own dependency track. A recipe can only enter an already
//! verified execution image; an image miss returns to resolution with the caller's environment.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use super::{PackageManifest, ResolvePlan, RootRunRecipe, schedule};
use crate::cargoless::{buildrs, config};
use crate::utils::content::FileStamp;

#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Request {
    input: PathBuf,
    cwd: PathBuf,
    bin: Option<String>,
    ignore_rust_version: bool,
    environment: [u8; 32],
    sysroot: Option<PathBuf>,
    generic_templates: bool,
    encoded_rustflags: Option<String>,
    threads: String,
}

#[derive(Serialize, Deserialize)]
struct Record {
    build_id: String,
    request: Request,
    recipe: RootRunRecipe,
    files: Vec<(PathBuf, Option<FileStamp>)>,
    trees: Vec<(PathBuf, [u8; 32])>,
    required: Vec<PathBuf>,
    scripts: Vec<BuildScript>,
    sysroot: PathBuf,
    sysroot_stamp: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct BuildScript {
    directory: PathBuf,
    source: PathBuf,
    package: String,
    registry: bool,
}

impl Request {
    pub(super) fn new(input: &Path, bin: Option<&str>, ignore_rust_version: bool) -> Option<Self> {
        Some(Self {
            input: std::path::absolute(input).ok()?,
            cwd: std::env::current_dir().ok()?,
            bin: bin.map(str::to_owned),
            ignore_rust_version,
            environment: config::launch_environment(),
            sysroot: crate::options::sysroot(),
            generic_templates: crate::options::generic_templates(),
            encoded_rustflags: crate::options::encoded_rustflags_append().map(str::to_owned),
            threads: crate::options::threads().ok()?,
        })
    }

    fn path(&self) -> Option<PathBuf> {
        let mut key = crate::store::entry::Key::new();
        key.part(&crate::utils::content::digest_hex(
            blake3::hash(&postcard::to_stdvec(self).ok()?).as_bytes(),
        ));
        Some(
            crate::store::LAUNCH
                .dir()
                .join(format!("{}.bin", key.digest())),
        )
    }

    pub(super) fn run(&self, program_args: &[String]) -> Option<ExitCode> {
        // Installation must visit every unit to repair missing templates. Diagnostic modes retain
        // the normal pipeline's cached build-script warnings and scheduling observations.
        if crate::cli::prepare_only()
            || crate::diag::verbose()
            || crate::options::debug_bldrs()
            || crate::options::debug_unify()
            || crate::options::no_ir_cache()
            || crate::options::no_base_image()
            || crate::options::no_deps_image()
            || crate::options::cless_jobs().is_err()
        {
            return None;
        }
        let start = std::time::Instant::now();
        let bytes = std::fs::read(self.path()?).ok()?;
        let (data, digest) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
        if blake3::hash(data).as_bytes().as_slice() != digest {
            return None;
        }
        let record: Record = postcard::from_bytes(data).ok()?;
        if !crate::store::entry::is_current_generation(&record.build_id)
            || record.request != *self
            || !record.current()
        {
            return None;
        }
        let mut argv = vec![record.recipe.argv0];
        argv.extend_from_slice(program_args);
        let previous: Vec<_> = record
            .recipe
            .env
            .iter()
            .map(|(name, _)| (name, std::env::var_os(name)))
            .collect();
        // SAFETY: single-threaded CLI startup, before compiler or guest threads start.
        unsafe {
            for (name, value) in &record.recipe.env {
                std::env::set_var(name, value);
            }
        }
        let result =
            crate::cli::try_prepared_launch(record.recipe.rustc_args, argv, start.elapsed());
        // A missing/corrupt execution image must resolve against the caller's environment, not
        // against values injected by an earlier build script.
        // SAFETY: the image-only driver has returned without starting compiler or guest threads.
        unsafe {
            for (name, value) in previous {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        result
    }
}

impl Record {
    fn current(&self) -> bool {
        if !self.files.iter().all(|(path, stamp)| match stamp {
            Some(stamp) => stamp.is_current(),
            None => absent(path),
        }) || !self.required.iter().all(|path| path.is_file())
            || !self
                .trees
                .iter()
                .all(|(path, stamp)| tree_stamp(path) == Some(*stamp))
            || self.scripts.iter().any(|script| {
                buildrs::should_rerun(
                    &script.directory,
                    script.registry,
                    &script.package,
                    &script.source,
                    &[],
                    &|name| std::env::var(name).ok(),
                )
                .0
            })
        {
            return false;
        }
        // Use the selector as well as image validation: an old generation can remain readable
        // after the selected toolchain or sysroot has changed.
        crate::sysroot::ensure_sysroot().is_ok_and(|path| path == self.sysroot)
            && crate::sysroot::current_stamp_value() == self.sysroot_stamp
    }
}

fn absent(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn tree_stamp(path: &Path) -> Option<[u8; 32]> {
    let stamp = schedule::source_stamp_dir(false, path, "launch input").ok()?;
    Some(*blake3::hash(stamp.as_bytes()).as_bytes())
}

/// Publish the recipe before entering the driver (which exits after execution). The record is
/// usable only when the corresponding execution image has also been successfully published.
#[allow(clippy::too_many_arguments)]
pub(super) fn store(
    request: Request,
    manifest: &PackageManifest,
    plan: &ResolvePlan,
    fps: &[String],
    layout: &schedule::Layout,
    root_fp: Option<&str>,
    sysroot: &Path,
    recipe: RootRunRecipe,
) -> Option<()> {
    let path = request.path()?;
    let mut paths = BTreeSet::new();
    let mut trees = BTreeSet::from([manifest.root.clone()]);
    let mut required = BTreeSet::new();
    let mut scripts = Vec::new();
    if request.input.is_file() {
        paths.insert(request.input.clone());
    }
    paths.insert(manifest.lock_root.join("Cargo.lock"));
    let mut roots = BTreeSet::from([manifest.root.clone(), manifest.lock_root.clone()]);
    for (unit, fp) in plan.units.iter().zip(fps) {
        roots.insert(unit.source_dir.clone());
        paths.insert(unit.source_dir.join("Cargo.toml"));
        required.insert(unit.lib_path.clone());
        if !unit.from_registry && unit.immutable_source_id.is_none() {
            trees.insert(unit.source_dir.clone());
        }
        if unit.has_build_script {
            let directory = layout.build_dir(&unit.package, fp);
            scripts.push(BuildScript {
                directory,
                source: unit.source_dir.clone(),
                package: unit.package.clone(),
                registry: unit.from_registry,
            });
        }
    }
    if manifest.has_build_script {
        scripts.push(BuildScript {
            directory: layout.build_dir(&manifest.name, root_fp?),
            source: manifest.root.clone(),
            package: manifest.name.clone(),
            registry: false,
        });
    }
    for script in &scripts {
        paths.extend([
            script.directory.join("rerun.txt"),
            script.directory.join("output.txt"),
        ]);
    }
    // Manifest discovery can walk beyond a path dependency, and an absent parent manifest or
    // configuration can appear later. Keep those absences rather than just the files read today.
    for root in &roots {
        for parent in root.ancestors() {
            paths.insert(parent.join("Cargo.toml"));
        }
    }
    for root in [&manifest.root, &manifest.lock_root] {
        let config = config::CargoConfig::load_at(root, config::cargo_home().as_deref()).ok()?;
        paths.extend(config.input_paths().iter().cloned());
    }
    // Rustflags also consult HOME independently of CARGO_HOME.
    paths.extend(crate::cargoless::rustflags::config_candidates(
        &manifest.root,
    ));
    let files = paths
        .into_iter()
        .map(|path| {
            let stamp = match FileStamp::of(&path) {
                Ok(stamp) => Some(stamp),
                Err(_) if absent(&path) => None,
                Err(_) => return None,
            };
            Some((path, stamp))
        })
        .collect::<Option<Vec<_>>>()?;
    // Parent trees already cover nested dependencies; avoid reading their bytes twice.
    let trees = trees
        .iter()
        .filter(|tree| {
            !trees
                .iter()
                .any(|parent| parent != *tree && tree.starts_with(parent))
        })
        .map(|tree| Some((tree.clone(), tree_stamp(tree)?)))
        .collect::<Option<Vec<_>>>()?;
    let record = Record {
        build_id: crate::options::build::BUILD_ID.into(),
        request,
        recipe,
        files,
        trees,
        required: required.into_iter().collect(),
        scripts,
        sysroot: sysroot.to_path_buf(),
        sysroot_stamp: crate::sysroot::current_stamp_value(),
    };
    let mut bytes = postcard::to_stdvec(&record).ok()?;
    bytes.extend_from_slice(blake3::hash(&bytes).as_bytes());
    std::fs::create_dir_all(crate::store::LAUNCH.dir()).ok()?;
    crate::store::publish_bytes(&path, &bytes).ok()
}
