//! The serde documents a package carries: META, STAMPS, NATIVELIBS, MC, RELOC and MODULE.
//!
//! The write view borrows the frozen region and the index tables so packing copies nothing; the read
//! view owns what a load deserializes. Their field names and order are part of the format.

use serde::{Deserialize, Serialize};

use super::Error;

/// META section: the package form of the L2 header metadata (build_id is promoted into the
/// container header).
#[derive(Serialize, Deserialize)]
pub(super) struct Meta {
    pub(super) args: Vec<String>,
    /// `env!`/`option_env!` dependencies (as in L2: (name, compile-time value; None = unset at
    /// compile time))
    pub(super) envs: Vec<(String, Option<String>)>,
    /// Always None today (full modules only; the delta+BASE form is reserved).
    pub(super) base_key: Option<String>,
    pub(super) target: String,
}

/// NATIVELIBS entry: a produced library. `path` serves only cross-checking and diagnostics; the
/// bytes needed for execution always travel with the package and are never read from that path.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct NativeLibEntry {
    pub(super) path: String,
    /// 0=static_archive 1=global_asm (same family for bin/dep; cache/global-asm directory)
    pub(super) role: u8,
    pub(super) fnv: u128,
    pub(super) bytes: Vec<u8>,
}

/// MC entry: raw bytes of a produced global_asm/dep_asm `.so`. At package load time it is loaded
/// in-process (mcload) without dlopen, and cross-checked against NATIVELIBS by fnv.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct McEntry {
    pub(super) fnv: u128,
    pub(super) bytes: Vec<u8>,
}

/// RELOC section: fixed-base requirement plus entry symbol (entry semantics live in
/// `Module.entry`; this field is informational).
#[derive(Serialize, Deserialize)]
pub(super) struct Reloc {
    pub(super) requires_fixed_base: bool,
    pub(super) entry: Box<str>,
}

pub(super) use crate::vm::archive::meta::ModuleMeta;
#[cfg(any(feature = "compiler", test))]
pub(super) use crate::vm::archive::meta::ModuleMetaRef;

/// The target this build runs on, spelled as [`Meta::target`] records it: the reader compares
/// against it and the writer stores it, so the two spellings are one fact.
pub(super) fn host_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The NATIVELIBS entry an MC image came from: the global_asm role with the same digest.
/// Load-time cross-checking and instantiation both ask this question.
pub(super) fn native_entry_for_mc(libs: &[NativeLibEntry], fnv: u128) -> Option<&NativeLibEntry> {
    libs.iter().find(|lib| lib.role == 1 && lib.fnv == fnv)
}

/// A library's bytes still hashing to the digest the container recorded for them. Both the loader's
/// cross-check and materialization ask it, and both report the same corruption.
pub(super) fn check_lib_hash(lib: &NativeLibEntry) -> Result<(), Error> {
    super::format::check_hash(
        &lib.bytes,
        lib.fnv,
        format!("package native library `{}` has wrong hash", lib.path),
    )
}

#[cfg(any(feature = "compiler", test))]
pub(super) fn postcard_bytes<T: Serialize>(v: &T) -> Result<Vec<u8>, Error> {
    postcard::to_stdvec(v).map_err(|e| Error::build(format!("cannot format the package: {e}")))
}
