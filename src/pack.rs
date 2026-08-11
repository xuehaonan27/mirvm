//! `.mirvm` 包格式 + pack/run（mode B，designs/modeb-mirvmar-design.md）。
//!
//! 路线 = D9b：包 = L2 engine-IR 缓存的可移植化（版本头/校验/重定位段 +
//! §7.22 机器码节预留）。单文件自包含：除 FFI 真外国库（glibc 类）外，运行期
//! 不依赖预存缓存、源码与 rustc/cargo 痕迹；包内自产动态库会按内容哈希自动
//! 物化。**格式当前不定死**
//! （2026-07-23 用户裁定，fmt_ver 只作同代区分，对外冻结归 D4 评审）。
//!
//! 容器版式（小端）：
//! ```text
//! magic "MIRVMAR\0" | fmt_ver u32 | build_id_len u32 + bytes
//! section_cnt u32 | 节表 ×N {tag u32, off u64, len u64, hash u128(fnv1a 双程)}
//! 节内容 | whole_hash u128（除本字段外全文件）
//! ```
//! 校验语义 = **refuse-loud，绝不静默重建**（包是分发物不是缓存）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"MIRVMAR\0";
const FMT_VER: u32 = 2;
const SECTION_ENTRY_LEN: usize = 36;
const WHOLE_HASH_LEN: usize = 16;

const TAG_META: u32 = 1;
const TAG_STAMPS: u32 = 2;
#[allow(dead_code)] // BASE 节预留（当前恒全量模块，不产出）
const TAG_BASE: u32 = 3;
const TAG_MODULE: u32 = 4;
const TAG_NATIVELIBS: u32 = 5;
const TAG_RELOC: u32 = 6;
const TAG_MC: u32 = 7;

/// fnv1a-128（双程异种子；校验强度与 L2 同族，格式演进时随评）。
fn hash128(data: &[u8]) -> u128 {
    let a = crate::lower::asm::fnv1a(data);
    let b = crate::lower::asm::fnv1a(
        b"\x01mirvmar"
            .iter()
            .chain(data.iter())
            .copied()
            .collect::<Vec<u8>>()
            .as_slice(),
    );
    ((a as u128) << 64) | b as u128
}

/// META 节：L2 Header 元信息的包形态（build_id 提升进容器头）。
#[derive(Serialize, Deserialize)]
struct Meta {
    args: Vec<String>,
    /// `env!`/`option_env!` 依赖（同 L2：(名, 编译时值；None = 编译时未设)）
    envs: Vec<(String, Option<String>)>,
    /// 当前恒 None（全量模块；delta+BASE 形态预留）
    base_key: Option<String>,
    target: String,
}

/// NATIVELIBS 节条目：自产库清单。`path` 只用于互证和诊断；执行所需字节
/// 始终随包携带，不再从该旧路径读取。
#[derive(Serialize, Deserialize)]
struct NativeLibEntry {
    path: String,
    /// 0=static_archive 1=global_asm（bin/dep 同族，cache/global-asm 域）
    role: u8,
    fnv: u128,
    bytes: Vec<u8>,
}

/// MC 节条目（片③）：自产 global_asm/dep_asm 族 `.so` 原始字节——装载时
/// 进程内自装载（mcload），不经 dlopen；与 NATIVELIBS 按 fnv 互证。
#[derive(Serialize, Deserialize)]
struct McEntry {
    fnv: u128,
    bytes: Vec<u8>,
}

/// RELOC 节：固定基要求 + 入口符号（entry 语义锚在 Module.entry，本字段供人读）。
#[derive(Serialize, Deserialize)]
struct Reloc {
    requires_fixed_base: bool,
    entry: Box<str>,
}

fn postcard_bytes<T: Serialize>(v: &T) -> Result<Vec<u8>, String> {
    postcard::to_stdvec(v).map_err(|e| format!("fail to format package: {e}"))
}

fn build_container(sections: &[(u32, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let bid = env!("MIRVM_BUILD_ID").as_bytes();
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
    if bid != env!("MIRVM_BUILD_ID") {
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

static NEXT_NATIVE_TEMP: AtomicU64 = AtomicU64::new(0);

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

    let serial = NEXT_NATIVE_TEMP.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        ".{:032x}.so.tmp-{}-{serial}",
        lib.fnv,
        std::process::id()
    ));
    std::fs::write(&tmp, &lib.bytes)
        .map_err(|e| format!("fail to materialize package native library: {e}"))?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("fail to publish package native library: {e}"));
    }
    Ok(path)
}

/// 打包入账（lower 刚完成、guest 未运行的洁净快照——与 L2 store 同一时机）。
/// 拒绝 = 文案（固定基缺失/native 库无法读取），调用方响亮终止。
pub(crate) fn write_package(
    tcx: rustc_middle::ty::TyCtxt<'_>,
    rustc_args: &[String],
    module: &crate::vm::engine::ir::Module,
    out: &Path,
) -> Result<(), String> {
    crate::vm::engine::verify::module(module)
        .map_err(|e| format!("refusing to package invalid bytecode: {e}"))?;
    // 固定基要求（L2 同契约：非固定基 = 快照内嵌地址跨进程无效，不产包）
    if !module.frozen.as_ref().is_some_and(|f| f.at_fixed_base()) {
        return Err("冻结区不在固定基址（并发抢占/ASLR 冲突）——重试打包".into());
    }
    if !module.entry_stub_sites.is_empty() && !module.entry_stubs.at_fixed_base() {
        return Err("条目 stub 域不在固定基址——重试打包".into());
    }
    // 输入戳和 env! 清单只作来源记录。可执行语义已经冻结进 Module；分发包运行时
    // 不应要求源码仍在原路径，也不应要求目标机器复刻编译环境。
    let (stamps, envs) = crate::ircache::collect_input_stamps(tcx).unwrap_or_default();
    let meta = Meta {
        args: rustc_args.to_vec(),
        envs,
        base_key: None,
        target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
    };
    let reloc = Reloc {
        requires_fixed_base: true,
        entry: "main".into(),
    };
    // NATIVELIBS：所有自产库字节随包携带。global_asm 族另入 MC 节走进程内
    // 装载；MIRVM_PACK_NO_MC=1 只切换装载方式，不再破坏包的自包含性。
    let ga_dir = crate::sysroot::cache_dir().join("global-asm");
    let ga_prefix = ga_dir.display().to_string();
    let no_mc = std::env::var_os("MIRVM_PACK_NO_MC").is_some();
    let mut libs = Vec::new();
    let mut mc_entries = Vec::new();
    for p in &module.required_native_libs {
        let data = std::fs::read(&**p).map_err(|e| format!("自产库 `{p}` 读取失败: {e}"))?;
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
    let module_bytes =
        postcard_bytes(&module).map_err(|e| format!("fail to serialize module: {e}"))?;

    let mut sections: Vec<(u32, Vec<u8>)> = vec![
        (TAG_META, postcard_bytes(&meta)?),
        (TAG_STAMPS, postcard_bytes(&stamps)?),
        (TAG_MODULE, module_bytes),
        (TAG_NATIVELIBS, postcard_bytes(&libs)?),
        (TAG_RELOC, postcard_bytes(&reloc)?),
    ];
    if !mc_entries.is_empty() {
        sections.push((TAG_MC, postcard_bytes(&mc_entries)?));
    }

    let buf = build_container(&sections)?;

    // 原子发布（全仓同款：临时名写全再 rename）
    let dir = out.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("fail to create package directory: {e}"))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        out.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, &buf).map_err(|e| format!("fail to write package: {e}"))?;
    std::fs::rename(&tmp, out).map_err(|e| format!("fail to release package: {e}"))?;
    Ok(())
}

/// 装载好的包（module 已恢复冻结区基址；调用方接 warm 后半段：
/// asm_sites 重物化 → run_vm_engine）。
pub(crate) struct LoadedPackage {
    pub module: crate::vm::engine::ir::Module,
}

/// 包嗅探：前 8 字节是 magic 即包（run 的岔路判据）。
pub(crate) fn is_package(path: &Path) -> bool {
    let mut b = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| {
            use std::io::Read as _;
            f.read_exact(&mut b)
        })
        .is_ok_and(|()| &b == MAGIC)
}

/// 装载 + 全校验（refuse-loud）。输入戳和编译时环境只作来源记录；包内 Module
/// 已冻结其语义，运行时不再要求源码或原编译环境在场。
pub(crate) fn load_package(path: &Path) -> Result<LoadedPackage, String> {
    let raw = std::fs::read(path).map_err(|e| format!("fail to read package: {e}"))?;
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
    let _: Vec<crate::ircache::FileStamp> = postcard::from_bytes(package.section(TAG_STAMPS)?)
        .map_err(|e| format!("STAMPS 节解析失败: {e}"))?;
    let libs: Vec<NativeLibEntry> = postcard::from_bytes(package.section(TAG_NATIVELIBS)?)
        .map_err(|e| format!("NATIVELIBS 节解析失败: {e}"))?;
    let mc_entries: Vec<McEntry> = if package.has_section(TAG_MC) {
        postcard::from_bytes(package.section(TAG_MC)?)
            .map_err(|e| format!("fail to resolve MC section: {e}"))?
    } else {
        Vec::new()
    };

    let reloc: Reloc = postcard::from_bytes(package.section(TAG_RELOC)?)
        .map_err(|e| format!("RELOC 节解析失败: {e}"))?;
    if &*reloc.entry != "main" {
        return Err(format!("unsupported package entry `{}`", reloc.entry));
    }
    let mut module: crate::vm::engine::ir::Module =
        postcard::from_bytes(package.section(TAG_MODULE)?)
            .map_err(|e| format!("MODULE 节解析失败（冻结区固定基恢复未成立？）: {e}"))?;
    crate::vm::engine::verify::module(&module)
        .map_err(|e| format!("MODULE bytecode verification failed: {e}"))?;
    if reloc.requires_fixed_base && !module.frozen.as_ref().is_some_and(|f| f.at_fixed_base()) {
        return Err("包要求固定基址但当前进程不可用（被占/ASLR 冲突）——重试或空闲后跑".into());
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
    let mut images = Vec::with_capacity(mc_entries.len());
    for mc in &mc_entries {
        if hash128(&mc.bytes) != mc.fnv {
            return Err("package MC entry has wrong content hash".into());
        }
        if !covered_hashes.insert(mc.fnv) {
            return Err("package MC section contains a duplicate image".into());
        }
        let Some(l) = libs.iter().find(|l| l.role == 1 && l.fnv == mc.fnv) else {
            return Err("包 MC 节含 NATIVELIBS 无互证条目（不符或多余）".into());
        };
        let img = crate::vm::engine::mcload::load(&mc.bytes)
            .map_err(|e| format!("fail to load MC image ({}): {e}", l.path))?;
        images.push(img);
    }
    module.mc_images = images;

    let mut required_native_libs = Vec::with_capacity(libs.len());
    for lib in &libs {
        if lib.role == 1 && covered_hashes.contains(&lib.fnv) {
            continue;
        }
        let path = materialize_native_blob_at(&crate::sysroot::cache_dir(), lib)?;
        required_native_libs.push(path.to_string_lossy().into_owned().into_boxed_str());
    }
    module.required_native_libs = required_native_libs;
    Ok(LoadedPackage { module })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replace_whole_hash(raw: &mut Vec<u8>) {
        raw.truncate(raw.len() - WHOLE_HASH_LEN);
        raw.extend_from_slice(&hash128(raw).to_le_bytes());
    }

    fn header_with_count(count: u32) -> Vec<u8> {
        let bid = env!("MIRVM_BUILD_ID").as_bytes();
        let mut body = Vec::new();
        body.extend_from_slice(MAGIC);
        body.extend_from_slice(&FMT_VER.to_le_bytes());
        body.extend_from_slice(&(bid.len() as u32).to_le_bytes());
        body.extend_from_slice(bid);
        body.extend_from_slice(&count.to_le_bytes());
        body.extend_from_slice(&hash128(&body).to_le_bytes());
        body
    }

    fn table_start() -> usize {
        MAGIC.len() + 4 + 4 + env!("MIRVM_BUILD_ID").len() + 4
    }

    #[test]
    fn parser_accepts_writer_output() {
        let raw = build_container(&[(TAG_META, vec![1, 2]), (99, vec![3, 4, 5])]).unwrap();
        let parsed = parse_container(&raw).unwrap();
        assert_eq!(parsed.section(TAG_META).unwrap(), [1, 2]);
        assert_eq!(parsed.section(99).unwrap(), [3, 4, 5]);
    }

    #[test]
    fn parser_rejects_truncated_build_id_without_panicking() {
        let mut body = Vec::new();
        body.extend_from_slice(MAGIC);
        body.extend_from_slice(&FMT_VER.to_le_bytes());
        body.extend_from_slice(&u32::MAX.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&hash128(&body).to_le_bytes());
        let err = parse_container(&body).err().unwrap();
        assert!(err.contains("build_id"), "{err}");
    }

    #[test]
    fn parser_bounds_section_count_before_allocating() {
        let raw = header_with_count(u32::MAX);
        let err = parse_container(&raw).err().unwrap();
        assert!(err.contains("section table"), "{err}");
    }

    #[test]
    fn parser_rejects_overflowing_section_range() {
        let mut raw = build_container(&[(TAG_META, vec![1])]).unwrap();
        let offset_pos = table_start() + 4;
        raw[offset_pos..offset_pos + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        replace_whole_hash(&mut raw);
        let err = parse_container(&raw).err().unwrap();
        assert!(
            err.contains("boundary") || err.contains("overflow"),
            "{err}"
        );
    }

    #[test]
    fn parser_rejects_duplicate_tags_and_overlapping_sections() {
        let original = build_container(&[(TAG_META, vec![1]), (TAG_MODULE, vec![2])]).unwrap();

        let mut duplicate = original.clone();
        let second_tag = table_start() + SECTION_ENTRY_LEN;
        duplicate[second_tag..second_tag + 4].copy_from_slice(&TAG_META.to_le_bytes());
        replace_whole_hash(&mut duplicate);
        assert!(
            parse_container(&duplicate)
                .err()
                .unwrap()
                .contains("duplicate")
        );

        let mut overlap = original;
        let first_offset = table_start() + 4;
        let first = overlap[first_offset..first_offset + 8].to_vec();
        let second_offset = table_start() + SECTION_ENTRY_LEN + 4;
        overlap[second_offset..second_offset + 8].copy_from_slice(&first);
        replace_whole_hash(&mut overlap);
        assert!(parse_container(&overlap).err().unwrap().contains("overlap"));
    }

    #[test]
    fn parser_checks_every_section_hash() {
        let mut raw = build_container(&[(99, vec![1, 2, 3])]).unwrap();
        let hash_pos = table_start() + 4 + 8 + 8;
        raw[hash_pos] ^= 1;
        replace_whole_hash(&mut raw);
        let err = parse_container(&raw).err().unwrap();
        assert!(err.contains("wrong hash"), "{err}");
    }

    #[test]
    fn native_blob_materialization_uses_embedded_bytes() {
        let root = std::env::temp_dir().join(format!(
            "mirvm-pack-test-{}-{}",
            std::process::id(),
            NEXT_NATIVE_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = b"embedded native image".to_vec();
        let lib = NativeLibEntry {
            path: "/path/that/does/not/exist.so".into(),
            role: 0,
            fnv: hash128(&bytes),
            bytes: bytes.clone(),
        };
        let path = materialize_native_blob_at(&root, &lib).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        std::fs::write(&path, b"corrupt").unwrap();
        assert_eq!(
            std::fs::read(materialize_native_blob_at(&root, &lib).unwrap()).unwrap(),
            lib.bytes
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
