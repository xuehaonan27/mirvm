//! `.mirvm` package format plus pack/run.
//!
//! A package is a portable form of the L2 engine-IR cache: version header, checksums, a
//! relocation section, and a reserved machine-code section. It is self-contained: apart from
//! genuine foreign libraries (glibc and friends) it depends at runtime on no pre-existing cache,
//! no source, and no rustc/cargo traces, and the dynamic libraries it produces are materialized
//! automatically by content hash. The format is not frozen yet (`fmt_ver` merely separates
//! generations).
//!
//! Container layout (little-endian):
//! ```text
//! magic "MIRVMAR\0" | fmt_ver u32 | build_id_len u32 + bytes
//! section_cnt u32 | section table ×N {tag u32, off u64, len u64, hash u128 (fnv1a, two passes)}
//! section contents | whole_hash u128 (whole file except this field)
//! ```
//! Verification = **refuse-loud, never silently rebuild** (a package is a distribution artifact,
//! not a cache).
//!
//! The work is split by the artifact's own structure: [`format`] is the container and the two
//! cursors that read and write it, [`meta`] the serde documents it carries, [`funcs`] the function
//! index, [`read`] and [`write`] the two directions, and [`native`] the store side of a package's
//! libraries.

mod format;
mod funcs;
mod meta;
mod native;
mod read;
#[cfg(test)]
mod tests;
mod write;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use meta::{McEntry, ModuleMeta, NativeLibEntry};
use native::materialize_native_blob_at;

// The three free functions the CLI reaches as `crate::pack::…`, the paths its branch criteria and
// pack step already name them by.
pub(crate) use read::{is_package, load_package};
pub(crate) use write::write_package;

/// Why a `.mirvm` package could not be produced or consumed.
///
/// One variant per failure class a caller can act on, not per message: `NotAPackage` means "treat
/// the input as something else", `Incompatible` means "re-pack with this build", `Corrupt` means the
/// artifact is unusable, `Reject` means the container is intact but its contents are refused, and
/// the two write classes name the path. The specific reason stays in `detail`, which is what a
/// reader reports and what the JSON `details` field carries.
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub enum Error {
    /// The input's magic says it is not a package at all.
    #[error("{detail}")]
    NotAPackage { detail: String },

    /// The container is damaged: truncation, a checksum that does not match, an offset out of
    /// bounds, a duplicate or missing section, or a function table that disagrees with the module.
    #[error("{detail}")]
    Corrupt { detail: String },

    /// The container parsed, but the contents were refused: bytecode verification, a requirement
    /// this host cannot satisfy, or an embedded image that will not load.
    #[error("{detail}")]
    Reject { detail: String },

    /// The package was produced by a different mirvm build or format version.
    #[error("{detail}")]
    Incompatible { detail: String },

    /// Writing a package from a module failed before touching the filesystem.
    #[error("{detail}")]
    Build { detail: String },

    /// Lowering the package's contents failed.
    #[error(transparent)]
    Lower(#[from] crate::lower::Error),

    /// A filesystem operation on a package or one of its artifacts failed. `detail` names the
    /// operation, because the same `io::Error` kind means different things at each step.
    #[error("{detail}: {source}")]
    Io {
        detail: String,
        #[serde(skip)]
        #[source]
        source: std::io::Error,
    },
}

crate::diag_codes! {
    Error: Pack => {
        NotAPackage => "pack.not_a_package",
        Corrupt => "pack.corrupt",
        Reject => "pack.reject",
        Incompatible => "pack.incompatible",
        Build => "pack.build",
        Lower => "pack.lower",
        Io => "pack.io",
    }
}

/// A constructor per single-detail class.
///
/// The variant list and the constructors are the same fact, so they are written once: a hand-written
/// constructor whose variant was later renamed would keep compiling and return the wrong class.
macro_rules! detail_constructors {
    ($( $constructor:ident => $variant:ident, )+) => {
        impl Error {
            $(
                fn $constructor(detail: impl Into<String>) -> Self {
                    Error::$variant {
                        detail: detail.into(),
                    }
                }
            )+
        }
    };
}

detail_constructors! {
    not_a_package => NotAPackage,
    corrupt => Corrupt,
    reject => Reject,
    incompatible => Incompatible,
    build => Build,
}

impl Error {
    fn io(detail: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            detail: detail.into(),
            source,
        }
    }
}

/// A validated, immutable package image. Each `instantiate` creates independent frozen memory and
/// machine-code images.
pub(crate) struct LoadedPackage {
    raw: std::sync::Arc<[u8]>,
    module_meta: ModuleMeta,
    function_blobs: Vec<crate::vm::ir::FuncBlob>,
    libs: Vec<NativeLibEntry>,
    mc_entries: Vec<McEntry>,
    heat_path: PathBuf,
}

/// A validated, immutable `.mirvm` artifact that can create multiple independent Engines.
pub struct Package {
    loaded: LoadedPackage,
}

impl Package {
    /// Copy and fully validate a package without allocating an Engine or executing guest code.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        load_package(path.as_ref()).map(|loaded| Self { loaded })
    }

    /// Create an independent Engine instance from this artifact.
    ///
    /// # Safety
    ///
    /// Package verification proves the container and VM bytecode shape, but cannot prove that
    /// embedded native libraries, foreign symbol declarations, and FFI signatures agree with
    /// the host process. The caller must trust those package inputs and ABI declarations.
    pub unsafe fn instantiate(&self) -> Result<crate::vm::Engine, Error> {
        let mut module = self.loaded.instantiate()?;
        module.asm_stub_addrs = crate::lower::asm::try_materialize(&module.asm_sites)?;
        module.finalize_entry_argv(&[]).map_err(Error::reject)?;
        unsafe { crate::vm::Engine::from_module_unchecked(module) }.map_err(Error::reject)
    }
}

impl LoadedPackage {
    pub(crate) fn instantiate(&self) -> Result<crate::vm::ir::Module, Error> {
        let mut module = self.module_meta.instantiate()?;
        let mut covered_hashes = HashSet::with_capacity(self.mc_entries.len());
        let mut images = Vec::with_capacity(self.mc_entries.len());
        for mc in &self.mc_entries {
            covered_hashes.insert(mc.fnv);
            let lib = meta::native_entry_for_mc(&self.libs, mc.fnv).ok_or_else(|| {
                Error::corrupt(format!(
                    "validated package lost the native entry for MC image {:032x}",
                    mc.fnv
                ))
            })?;
            let image = crate::vm::mcload::load(&mc.bytes).map_err(|e| {
                Error::reject(format!("cannot load the MC image ({}): {e}", lib.path))
            })?;
            images.push(image);
        }
        module.mc_images = images;

        let mut required_native_libs = Vec::with_capacity(self.libs.len());
        let mut required_native_hashes = Vec::with_capacity(self.libs.len());
        for lib in &self.libs {
            if lib.role == 1 && covered_hashes.contains(&lib.fnv) {
                continue;
            }
            let path = materialize_native_blob_at(&crate::store::PACKAGE_NATIVE.dir(), lib)?;
            required_native_libs.push(path.to_string_lossy().into_owned().into_boxed_str());
            required_native_hashes.push(lib.fnv);
        }
        module.required_native_libs = required_native_libs;
        module.required_native_hashes = required_native_hashes;
        module.funcs = crate::vm::ir::FuncTable::from_bytes(
            self.raw.clone(),
            self.function_blobs.clone(),
            self.heat_path.clone(),
        );
        Ok(module)
    }
}
