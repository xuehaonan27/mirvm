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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"MIRVMAR\0";
const FMT_VER: u32 = 4;
const SECTION_ENTRY_LEN: usize = 36;
const WHOLE_HASH_LEN: usize = 16;

const TAG_META: u32 = 1;
const TAG_STAMPS: u32 = 2;
#[allow(dead_code)] // BASE section reserved; modules are always full, so it is never produced.
const TAG_BASE: u32 = 3;
const TAG_MODULE: u32 = 4;
const TAG_NATIVELIBS: u32 = 5;
const TAG_RELOC: u32 = 6;
const TAG_MC: u32 = 7;
const TAG_FUNCS: u32 = 8;
const FUNC_ENTRY_LEN: usize = 32;

/// fnv1a-128: two passes with different seeds, the same checksum family as L2.
fn hash128(data: &[u8]) -> u128 {
    let a = crate::utils::content::fnv1a(data);
    let mut b = 0xcbf2_9ce4_8422_2325u64;
    for byte in b"\x01mirvmar".iter().chain(data) {
        b ^= u64::from(*byte);
        b = b.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ((a as u128) << 64) | b as u128
}

/// META section: the package form of the L2 header metadata (build_id is promoted into the
/// container header).
#[derive(Serialize, Deserialize)]
struct Meta {
    args: Vec<String>,
    /// `env!`/`option_env!` dependencies (as in L2: (name, compile-time value; None = unset at
    /// compile time))
    envs: Vec<(String, Option<String>)>,
    /// Always None today (full modules only; the delta+BASE form is reserved).
    base_key: Option<String>,
    target: String,
}

/// NATIVELIBS entry: a produced library. `path` serves only cross-checking and diagnostics; the
/// bytes needed for execution always travel with the package and are never read from that path.
#[derive(Clone, Serialize, Deserialize)]
struct NativeLibEntry {
    path: String,
    /// 0=static_archive 1=global_asm (same family for bin/dep; cache/global-asm directory)
    role: u8,
    fnv: u128,
    bytes: Vec<u8>,
}

/// MC entry: raw bytes of a produced global_asm/dep_asm `.so`. At package load time it is loaded
/// in-process (mcload) without dlopen, and cross-checked against NATIVELIBS by fnv.
#[derive(Clone, Serialize, Deserialize)]
struct McEntry {
    fnv: u128,
    bytes: Vec<u8>,
}

/// RELOC section: fixed-base requirement plus entry symbol (entry semantics live in
/// `Module.entry`; this field is informational).
#[derive(Serialize, Deserialize)]
struct Reloc {
    requires_fixed_base: bool,
    entry: Box<str>,
}

/// MODULE v4 stores only non-function metadata. A borrowed write form avoids copying the frozen
/// region and index tables.
#[derive(Serialize)]
struct ModuleMetaRef<'a> {
    function_names: &'a [Box<str>],
    exports: &'a std::collections::HashMap<Box<str>, crate::vm::ir::FuncId>,
    frozen: Option<crate::vm::frozen::FrozenSnapshot>,
    link_fn_addrs: &'a std::collections::HashMap<crate::vm::ir::LinkAddr, crate::vm::ir::FuncId>,
    native_libs: &'a [Box<str>],
    required_native_libs: &'a [Box<str>],
    tls: &'a [crate::vm::ir::TlsSlot],
    asm_stub_addrs: &'a [u64],
    asm_sites: &'a [crate::vm::ir::AsmSite],
    foreign_syms: &'a [crate::vm::ir::GotSym],
    got_fixups: &'a [crate::vm::ir::GotFixup],
    frozen_relocs: &'a [crate::vm::ir::FrozenReloc],
    entry_stub_sites: Vec<crate::vm::ir::EntryStubSite>,
    custom_alloc_shims: Option<crate::vm::ir::AllocShims>,
    guest_panic_cleanup: Option<crate::vm::ir::GuestPanicCleanup>,
    entry: Option<crate::vm::ir::EntryPlan>,
}

impl<'a> From<&'a crate::vm::ir::Module> for ModuleMetaRef<'a> {
    fn from(module: &'a crate::vm::ir::Module) -> Self {
        Self {
            function_names: &module.function_names,
            exports: &module.exports,
            frozen: module.frozen.as_ref().map(|frozen| {
                frozen
                    .to_snapshot()
                    .expect("package preflight checked frozen base")
            }),
            link_fn_addrs: &module.link_fn_addrs,
            native_libs: &module.native_libs,
            required_native_libs: &module.required_native_libs,
            tls: &module.tls,
            asm_stub_addrs: &module.asm_stub_addrs,
            asm_sites: &module.asm_sites,
            foreign_syms: &module.foreign_syms,
            got_fixups: &module.got_fixups,
            frozen_relocs: &module.frozen_relocs,
            entry_stub_sites: module
                .entry_stub_sites
                .iter()
                .chain(
                    module
                        .image_entry_stubs
                        .iter()
                        .flat_map(|(_, sites, _)| sites.iter()),
                )
                .cloned()
                .collect(),
            custom_alloc_shims: module.custom_alloc_shims,
            guest_panic_cleanup: module.guest_panic_cleanup,
            entry: module.entry,
        }
    }
}

#[derive(Clone, Deserialize)]
struct ModuleMeta {
    function_names: Vec<Box<str>>,
    exports: std::collections::HashMap<Box<str>, crate::vm::ir::FuncId>,
    frozen: Option<crate::vm::frozen::FrozenSnapshot>,
    link_fn_addrs: std::collections::HashMap<crate::vm::ir::LinkAddr, crate::vm::ir::FuncId>,
    native_libs: Vec<Box<str>>,
    required_native_libs: Vec<Box<str>>,
    tls: Vec<crate::vm::ir::TlsSlot>,
    asm_stub_addrs: Vec<u64>,
    asm_sites: Vec<crate::vm::ir::AsmSite>,
    foreign_syms: Vec<crate::vm::ir::GotSym>,
    got_fixups: Vec<crate::vm::ir::GotFixup>,
    frozen_relocs: Vec<crate::vm::ir::FrozenReloc>,
    entry_stub_sites: Vec<crate::vm::ir::EntryStubSite>,
    custom_alloc_shims: Option<crate::vm::ir::AllocShims>,
    guest_panic_cleanup: Option<crate::vm::ir::GuestPanicCleanup>,
    entry: Option<crate::vm::ir::EntryPlan>,
}

impl ModuleMeta {
    fn instantiate(&self) -> Result<crate::vm::ir::Module, String> {
        let frozen = self
            .frozen
            .as_ref()
            .map(crate::vm::frozen::FrozenArena::restore_dynamic)
            .transpose()?;
        let mut module = crate::vm::ir::Module {
            funcs: Default::default(),
            function_names: self.function_names.clone(),
            exports: self.exports.clone(),
            frozen,
            load_map: Default::default(),
            fn_addrs: self
                .link_fn_addrs
                .iter()
                .map(|(&addr, &func)| (addr.0, func))
                .collect(),
            link_fn_addrs: self.link_fn_addrs.clone(),
            executable_entry_addrs: Default::default(),
            native_libs: self.native_libs.clone(),
            required_native_libs: self.required_native_libs.clone(),
            required_native_hashes: Vec::new(),
            native_images: Vec::new(),
            mc_images: Vec::new(),
            tls: self.tls.clone(),
            asm_stub_addrs: self.asm_stub_addrs.clone(),
            asm_sites: self.asm_sites.clone(),
            foreign_syms: self.foreign_syms.clone(),
            got_fixups: self.got_fixups.clone(),
            frozen_relocs: self.frozen_relocs.clone(),
            entry_stub_sites: self.entry_stub_sites.clone(),
            entry_stubs: Default::default(),
            image_entry_stubs: Vec::new(),
            custom_alloc_shims: self.custom_alloc_shims,
            guest_panic_cleanup: self.guest_panic_cleanup,
            entry: self.entry,
            image_frozens: Vec::new(),
            backtrace_ips: Vec::new(),
            backtrace_image: None,
        };
        module.rebuild_load_map();
        module.load_map.require_mapped();
        Ok(module)
    }
}

fn postcard_bytes<T: Serialize>(v: &T) -> Result<Vec<u8>, String> {
    postcard::to_stdvec(v).map_err(|e| format!("fail to format package: {e}"))
}

fn build_function_section(funcs: &crate::vm::ir::FuncTable) -> Result<Vec<u8>, String> {
    let count = u32::try_from(funcs.len()).map_err(|_| "too many functions in package")?;
    let table_len = funcs
        .len()
        .checked_mul(FUNC_ENTRY_LEN)
        .and_then(|len| len.checked_add(4))
        .ok_or("package function table is too large")?;
    let mut encoded = Vec::with_capacity(funcs.len());
    let mut offset = table_len;
    for body in funcs {
        let bytes = postcard_bytes(body)?;
        let end = offset
            .checked_add(bytes.len())
            .ok_or("package function data is too large")?;
        encoded.push((offset, bytes));
        offset = end;
    }
    let mut out = Vec::with_capacity(offset);
    out.extend_from_slice(&count.to_le_bytes());
    for (offset, bytes) in &encoded {
        out.extend_from_slice(
            &u64::try_from(*offset)
                .map_err(|_| "package function offset is too large")?
                .to_le_bytes(),
        );
        out.extend_from_slice(
            &u64::try_from(bytes.len())
                .map_err(|_| "package function is too large")?
                .to_le_bytes(),
        );
        out.extend_from_slice(&hash128(bytes).to_le_bytes());
    }
    for (_, bytes) in encoded {
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

fn parse_function_section(
    section: &[u8],
    mapped_offset: usize,
) -> Result<Vec<crate::vm::ir::FuncBlob>, String> {
    let mut cursor = Cursor {
        data: section,
        pos: 0,
    };
    let count = usize::try_from(cursor.u32("function count")?)
        .map_err(|_| "package function count does not fit this host")?;
    let table_end = count
        .checked_mul(FUNC_ENTRY_LEN)
        .and_then(|len| len.checked_add(4))
        .ok_or("package function table length overflow")?;
    if table_end > section.len() {
        return Err("package function table is truncated or too large".into());
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count)
        .map_err(|_| "package function index is too large for available memory")?;
    for index in 0..count {
        let start = usize::try_from(cursor.u64("function offset")?)
            .map_err(|_| format!("function {index} offset does not fit this host"))?;
        let len = usize::try_from(cursor.u64("function length")?)
            .map_err(|_| format!("function {index} length does not fit this host"))?;
        let expected = cursor.u128("function hash")?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| format!("function {index} range overflow"))?;
        if start < table_end || end > section.len() {
            return Err(format!("function {index} crossed FUNCS section boundary"));
        }
        if hash128(&section[start..end]) != expected {
            return Err(format!("function {index} has wrong hash"));
        }
        entries.push((index, start, end, expected));
    }
    let mut sorted = entries.clone();
    sorted.sort_unstable_by_key(|entry| entry.1);
    if let Some(pair) = sorted.windows(2).find(|pair| pair[1].1 < pair[0].2) {
        return Err(format!(
            "functions {} and {} overlap in FUNCS section",
            pair[0].0, pair[1].0
        ));
    }
    entries
        .into_iter()
        .map(|(_, start, end, expected_hash)| {
            Ok(crate::vm::ir::FuncBlob {
                start: mapped_offset
                    .checked_add(start)
                    .ok_or("mapped function offset overflow")?,
                end: mapped_offset
                    .checked_add(end)
                    .ok_or("mapped function end overflow")?,
                expected_hash,
            })
        })
        .collect()
}

fn build_container(sections: &[(u32, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let bid = crate::options::build::BUILD_ID.as_bytes();
    let bid_len = u32::try_from(bid.len()).map_err(|_| "package build_id too long")?;
    let section_count =
        u32::try_from(sections.len()).map_err(|_| "too many sections in package")?;
    let table_len = sections
        .len()
        .checked_mul(SECTION_ENTRY_LEN)
        .ok_or("package section table is too large")?;
    let data_start = MAGIC
        .len()
        .checked_add(4 + 4)
        .and_then(|n| n.checked_add(bid.len()))
        .and_then(|n| n.checked_add(4))
        .and_then(|n| n.checked_add(table_len))
        .ok_or("package size overflow")?;

    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&FMT_VER.to_le_bytes());
    buf.extend_from_slice(&bid_len.to_le_bytes());
    buf.extend_from_slice(bid);
    buf.extend_from_slice(&section_count.to_le_bytes());

    let mut off = u64::try_from(data_start).map_err(|_| "package offset overflow")?;
    for (tag, data) in sections {
        let len = u64::try_from(data.len()).map_err(|_| "package section is too large")?;
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&off.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&hash128(data).to_le_bytes());
        off = off.checked_add(len).ok_or("package size overflow")?;
    }
    for (_, data) in sections {
        buf.extend_from_slice(data);
    }
    let whole = hash128(&buf);
    buf.extend_from_slice(&whole.to_le_bytes());
    Ok(buf)
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| format!("package {what} length overflow"))?;
        let value = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| format!("package truncated while reading {what}"))?;
        self.pos = end;
        Ok(value)
    }

    fn u32(&mut self, what: &str) -> Result<u32, String> {
        Ok(u32::from_le_bytes(
            self.take(4, what)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self, what: &str) -> Result<u64, String> {
        Ok(u64::from_le_bytes(
            self.take(8, what)?.try_into().expect("eight bytes"),
        ))
    }

    fn u128(&mut self, what: &str) -> Result<u128, String> {
        Ok(u128::from_le_bytes(
            self.take(16, what)?.try_into().expect("sixteen bytes"),
        ))
    }
}

struct ParsedPackage<'a> {
    sections: Vec<(u32, &'a [u8])>,
}

impl<'a> ParsedPackage<'a> {
    fn section(&self, tag: u32) -> Result<&'a [u8], String> {
        self.sections
            .iter()
            .find_map(|(found, data)| (*found == tag).then_some(*data))
            .ok_or_else(|| format!("package must have section with tag={tag}"))
    }

    fn has_section(&self, tag: u32) -> bool {
        self.sections.iter().any(|(found, _)| *found == tag)
    }
}

fn parse_container(raw: &[u8]) -> Result<ParsedPackage<'_>, String> {
    const MIN_LEN: usize = 8 + 4 + 4 + 4 + WHOLE_HASH_LEN;
    if raw.len() < MIN_LEN || raw.get(..MAGIC.len()) != Some(MAGIC) {
        return Err("not .mirvm package (mismatched or truncated magic header)".into());
    }
    let body_len = raw
        .len()
        .checked_sub(WHOLE_HASH_LEN)
        .ok_or("package is shorter than its hash trailer")?;
    let (body, whole) = raw.split_at(body_len);
    let recorded_hash = u128::from_le_bytes(whole.try_into().expect("sixteen-byte trailer"));
    if recorded_hash != hash128(body) {
        return Err("package content hash mismatched (broken or truncated)".into());
    }

    let mut cur = Cursor {
        data: body,
        pos: MAGIC.len(),
    };
    let package_ver = cur.u32("format version")?;
    if package_ver != FMT_VER {
        return Err(format!(
            "wrong package format version (package={package_ver}, mirvm={FMT_VER})"
        ));
    }
    let bid_len = usize::try_from(cur.u32("build_id length")?)
        .map_err(|_| "package build_id length does not fit this host")?;
    let bid = std::str::from_utf8(cur.take(bid_len, "build_id")?)
        .map_err(|e| format!("invalid package build_id: {e}"))?;
    if bid != crate::options::build::BUILD_ID {
        return Err("package build_id mismatch with current mirvm".into());
    }

    let count = usize::try_from(cur.u32("section count")?)
        .map_err(|_| "package section count does not fit this host")?;
    let table_len = count
        .checked_mul(SECTION_ENTRY_LEN)
        .ok_or("package section table length overflow")?;
    let data_start = cur
        .pos
        .checked_add(table_len)
        .filter(|end| *end <= body.len())
        .ok_or("package section table is truncated or too large")?;

    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count)
        .map_err(|_| "package section table is too large for available memory")?;
    let mut tags = HashSet::new();
    tags.try_reserve(count)
        .map_err(|_| "package section tag table is too large for available memory")?;
    for index in 0..count {
        let tag = cur.u32("section tag")?;
        if !tags.insert(tag) {
            return Err(format!("package has duplicate section tag={tag}"));
        }
        let off = usize::try_from(cur.u64("section offset")?)
            .map_err(|_| format!("package section {index} offset does not fit this host"))?;
        let len = usize::try_from(cur.u64("section length")?)
            .map_err(|_| format!("package section {index} length does not fit this host"))?;
        let expected_hash = cur.u128("section hash")?;
        let end = off
            .checked_add(len)
            .ok_or_else(|| format!("package section with tag={tag} range overflow"))?;
        if off < data_start || end > body.len() {
            return Err(format!(
                "package section with tag={tag} crossed its boundary"
            ));
        }
        entries.push((tag, off, end, expected_hash));
    }

    drop(tags);
    entries.sort_unstable_by_key(|(_, start, _, _)| *start);
    for pair in entries.windows(2) {
        if pair[1].1 < pair[0].2 {
            return Err(format!(
                "package sections with tag={} and tag={} overlap",
                pair[0].0, pair[1].0
            ));
        }
    }

    let mut sections = Vec::new();
    sections
        .try_reserve_exact(entries.len())
        .map_err(|_| "package section index is too large for available memory")?;
    for (tag, start, end, expected_hash) in entries {
        let data = &body[start..end];
        if hash128(data) != expected_hash {
            return Err(format!(
                "package section with tag={tag} has wrong hash value"
            ));
        }
        sections.push((tag, data));
    }
    Ok(ParsedPackage { sections })
}

fn materialize_native_blob_at(root: &Path, lib: &NativeLibEntry) -> Result<PathBuf, String> {
    if hash128(&lib.bytes) != lib.fnv {
        return Err(format!(
            "package native library `{}` has wrong hash",
            lib.path
        ));
    }
    let dir = root.join("package-native");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("fail to create package native directory: {e}"))?;
    let path = dir.join(format!("{:032x}.so", lib.fnv));
    if std::fs::read(&path)
        .ok()
        .is_some_and(|bytes| hash128(&bytes) == lib.fnv)
    {
        return Ok(path);
    }
    crate::store::publish_bytes(&path, &lib.bytes)
        .map_err(|e| format!("fail to publish package native library: {e}"))?;
    Ok(path)
}

/// Package entry: the clean snapshot right after lowering completes and before the guest runs, the
/// same point as the L2 store. Rejection is a plain reason string (missing fixed base, unreadable
/// native library); the caller aborts loudly.
pub(crate) fn write_package(
    tcx: rustc_middle::ty::TyCtxt<'_>,
    rustc_args: &[String],
    module: &crate::vm::ir::Module,
    out: &Path,
) -> Result<(), String> {
    crate::vm::verify::module(module)
        .map_err(|e| format!("refusing to package invalid bytecode: {e}"))?;
    // Fixed-base requirement (same contract as L2): without a fixed base the snapshot's embedded
    // addresses are invalid across processes, so no package is produced.
    if !module.frozen.as_ref().is_some_and(|f| f.at_fixed_base()) {
        return Err(
            "frozen region is not at a fixed base (concurrent claim/ASLR conflict); retry packing"
                .into(),
        );
    }
    if !module.entry_stub_sites.is_empty() && !module.entry_stubs.at_fixed_base() {
        return Err("entry stub region is not at a fixed base; retry packing".into());
    }
    // Input stamps and the env! list are provenance only. Executable semantics are already frozen
    // into the Module, so running a distributed package must not require the source at its original
    // path, nor a replica of the build environment on the target machine.
    let (stamps, envs) = crate::ircache::collect_input_stamps(tcx).unwrap_or_default();
    let meta = Meta {
        args: rustc_args.to_vec(),
        envs,
        base_key: None,
        target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
    };
    let reloc = Reloc {
        requires_fixed_base: false,
        entry: "main".into(),
    };
    // NATIVELIBS: every produced library's bytes travel with the package. The global_asm family
    // also enters the MC section for in-process loading; MIRVM_PACK_NO_MC=1 only switches the load
    // method and no longer breaks the package's self-containment.
    let ga_dir = crate::options::get().cache_root().join("global-asm");
    let ga_prefix = ga_dir.display().to_string();
    let no_mc = crate::options::get().pack_no_mc;
    let mut libs = Vec::new();
    let mut mc_entries = Vec::new();
    for p in &module.required_native_libs {
        let data = std::fs::read(&**p)
            .map_err(|e| format!("failed to read produced library `{p}`: {e}"))?;
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
    let module_bytes = postcard_bytes(&ModuleMetaRef::from(module))
        .map_err(|e| format!("fail to serialize module metadata: {e}"))?;
    let function_bytes = build_function_section(&module.funcs)?;

    let mut sections: Vec<(u32, Vec<u8>)> = vec![
        (TAG_META, postcard_bytes(&meta)?),
        (TAG_STAMPS, postcard_bytes(&stamps)?),
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
    std::fs::create_dir_all(dir).map_err(|e| format!("fail to create package directory: {e}"))?;
    crate::store::publish_bytes(out, &buf).map_err(|e| format!("fail to release package: {e}"))?;
    Ok(())
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
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        load_package(path.as_ref()).map(|loaded| Self { loaded })
    }

    /// Create an independent Engine instance from this artifact.
    ///
    /// # Safety
    ///
    /// Package verification proves the container and VM bytecode shape, but cannot prove that
    /// embedded native libraries, foreign symbol declarations, and FFI signatures agree with
    /// the host process. The caller must trust those package inputs and ABI declarations.
    pub unsafe fn instantiate(&self) -> Result<crate::vm::Engine, String> {
        let mut module = self.loaded.instantiate()?;
        module.asm_stub_addrs = crate::lower::asm::try_materialize(&module.asm_sites)?;
        module.finalize_entry_argv(&[])?;
        unsafe { crate::vm::Engine::from_module_unchecked(module) }
    }
}

impl LoadedPackage {
    pub(crate) fn instantiate(&self) -> Result<crate::vm::ir::Module, String> {
        let mut module = self.module_meta.instantiate()?;
        let mut covered_hashes = HashSet::with_capacity(self.mc_entries.len());
        let mut images = Vec::with_capacity(self.mc_entries.len());
        for mc in &self.mc_entries {
            covered_hashes.insert(mc.fnv);
            let lib = self
                .libs
                .iter()
                .find(|lib| lib.role == 1 && lib.fnv == mc.fnv)
                .ok_or_else(|| {
                    format!(
                        "validated package lost the native entry for MC image {:032x}",
                        mc.fnv
                    )
                })?;
            let image = crate::vm::mcload::load(&mc.bytes)
                .map_err(|e| format!("fail to load MC image ({}): {e}", lib.path))?;
            images.push(image);
        }
        module.mc_images = images;

        let mut required_native_libs = Vec::with_capacity(self.libs.len());
        let mut required_native_hashes = Vec::with_capacity(self.libs.len());
        for lib in &self.libs {
            if lib.role == 1 && covered_hashes.contains(&lib.fnv) {
                continue;
            }
            let path = materialize_native_blob_at(&crate::options::get().cache_root(), lib)?;
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

/// Load + full verification (refuse-loud). Input stamps and the compile-time environment are
/// provenance only; the Module inside has its semantics frozen, so running it no longer requires the
/// source or the original build environment.
pub(crate) fn load_package(path: &Path) -> Result<LoadedPackage, String> {
    // `Package::load` is safe, so its result must not retain the filesystem's mutable inode.
    // Copy once, then validate and lazily decode exclusively from this immutable snapshot.
    let raw: std::sync::Arc<[u8]> = std::fs::read(path)
        .map_err(|e| format!("fail to read package: {e}"))?
        .into();
    let package = parse_container(&raw)?;
    let meta: Meta = postcard::from_bytes(package.section(TAG_META)?)
        .map_err(|e| format!("fail to resolve META section: {e}"))?;
    let current_target = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
    if meta.target != current_target {
        return Err(format!(
            "package target mismatch (package={}, current={current_target})",
            meta.target
        ));
    }
    if meta.base_key.is_some() || package.has_section(TAG_BASE) {
        return Err("package BASE/delta form is not supported by this mirvm".into());
    }
    let _: Vec<crate::utils::content::FileStamp> =
        postcard::from_bytes(package.section(TAG_STAMPS)?)
            .map_err(|e| format!("failed to resolve STAMPS section: {e}"))?;
    let libs: Vec<NativeLibEntry> = postcard::from_bytes(package.section(TAG_NATIVELIBS)?)
        .map_err(|e| format!("failed to resolve NATIVELIBS section: {e}"))?;
    let mc_entries: Vec<McEntry> = if package.has_section(TAG_MC) {
        postcard::from_bytes(package.section(TAG_MC)?)
            .map_err(|e| format!("fail to resolve MC section: {e}"))?
    } else {
        Vec::new()
    };

    let reloc: Reloc = postcard::from_bytes(package.section(TAG_RELOC)?)
        .map_err(|e| format!("failed to resolve RELOC section: {e}"))?;
    if &*reloc.entry != "main" {
        return Err(format!("unsupported package entry `{}`", reloc.entry));
    }
    let module_meta: ModuleMeta = postcard::from_bytes(package.section(TAG_MODULE)?)
        .map_err(|e| format!("fail to resolve MODULE section: {e}"))?;
    let module = module_meta.instantiate()?;
    let function_section = package.section(TAG_FUNCS)?;
    let mapped_offset = function_section.as_ptr() as usize - raw.as_ptr() as usize;
    let function_blobs = parse_function_section(function_section, mapped_offset)?;
    if module.function_names.len() != function_blobs.len() {
        return Err(format!(
            "MODULE function name table has {} entries, expected {}",
            module.function_names.len(),
            function_blobs.len()
        ));
    }
    crate::vm::verify::module_header_with_count(&module, function_blobs.len())
        .map_err(|e| format!("MODULE bytecode verification failed: {e}"))?;
    // Before any MC/native materialization, every function gets full semantic verification.
    // Temporary objects are dropped each round; the run phase still decodes on demand from the owned
    // snapshot instead of keeping every function resident.
    let mut main_boundaries = 0;
    let mut main_catchers = 0;
    for (index, blob) in function_blobs.iter().enumerate() {
        let body: crate::vm::ir::FuncBody = postcard::from_bytes(&raw[blob.start..blob.end])
            .map_err(|e| format!("function {index} decode failed during verification: {e}"))?;
        crate::vm::verify::function_with_count(&module, function_blobs.len(), index, &body)
            .map_err(|e| format!("MODULE bytecode verification failed: {e}"))?;
        let (boundaries, catchers) = crate::vm::verify::body_main_role_counts(&body);
        main_boundaries += boundaries;
        main_catchers += catchers;
    }
    crate::vm::verify::main_role_counts(&module, main_boundaries, main_catchers)
        .map_err(|e| format!("MODULE bytecode verification failed: {e}"))?;
    if reloc.requires_fixed_base {
        return Err("package v4 cannot require a fixed runtime base".into());
    }
    if module.required_native_libs.len() != libs.len()
        || module
            .required_native_libs
            .iter()
            .zip(&libs)
            .any(|(path, lib)| path.as_ref() != lib.path.as_str())
    {
        return Err("package NATIVELIBS does not match MODULE native library order".into());
    }
    for lib in &libs {
        if lib.role > 1 {
            return Err(format!(
                "package native library `{}` has unknown role {}",
                lib.path, lib.role
            ));
        }
        if hash128(&lib.bytes) != lib.fnv {
            return Err(format!(
                "package native library `{}` has wrong hash",
                lib.path
            ));
        }
    }

    let mut covered_hashes = HashSet::with_capacity(mc_entries.len());
    for mc in &mc_entries {
        if hash128(&mc.bytes) != mc.fnv {
            return Err("package MC entry has wrong content hash".into());
        }
        if !covered_hashes.insert(mc.fnv) {
            return Err("package MC section contains a duplicate image".into());
        }
        let Some(_) = libs.iter().find(|l| l.role == 1 && l.fnv == mc.fnv) else {
            return Err(
                "package MC section has no matching NATIVELIBS entry (missing or extra)".into(),
            );
        };
    }
    let heat_key = format!("{:032x}", hash128(function_section));
    let heat_path = crate::options::get()
        .cache_root()
        .join("package-heat")
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

#[cfg(test)]
mod tests;
