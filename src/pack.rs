//! `.mirvm` 包格式 v0 + pack/run（mode B 片②，designs/modeb-mirvmar-design.md）。
//!
//! 路线 = D9b：包 = L2 engine-IR 缓存的可移植化（版本头/校验/重定位段 +
//! §7.22 机器码节预留）。单文件自包含：除 FFI 真外国库（glibc 类）外，
//! 运行期不读 `$HOME/.mirvm` 与 rustc/cargo 痕迹。**格式当前不定死**
//! （2026-07-23 用户裁定，fmt_ver 只作同代区分，对外冻结归 D4 评审）。
//!
//! 容器版式（小端）：
//! ```text
//! magic "MIRVMAR\0" | fmt_ver u32 | build_id_len u32 + bytes
//! section_cnt u32 | 节表 ×N {tag u32, off u64, len u64, hash u128(fnv1a 双程)}
//! 节内容 | whole_hash u128（除本字段外全文件）
//! ```
//! 校验语义 = **refuse-loud，绝不静默重建**（包是分发物不是缓存）。

use std::path::Path;

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"MIRVMAR\0";
const FMT_VER: u32 = 1;

const TAG_META: u32 = 1;
const TAG_STAMPS: u32 = 2;
#[allow(dead_code)] // BASE 节预留（当前恒全量模块，不产出）
const TAG_BASE: u32 = 3;
const TAG_MODULE: u32 = 4;
const TAG_NATIVELIBS: u32 = 5;
const TAG_RELOC: u32 = 6;
#[allow(dead_code)] // 片③ 机器码节预留
const TAG_MC: u32 = 7;

/// fnv1a-128（双程异种子；v0 校验强度与 L2 同族，格式演进时随评）。
fn hash128(data: &[u8]) -> u128 {
    let a = crate::lower::asm::fnv1a(data);
    let b = crate::lower::asm::fnv1a(b"\x01mirvmar".iter().chain(data.iter()).copied().collect::<Vec<u8>>().as_slice());
    ((a as u128) << 64) | b as u128
}

/// META 节：L2 Header 元信息的包形态（build_id 提升进容器头）。
#[derive(Serialize, Deserialize)]
struct Meta {
    args: Vec<String>,
    /// `env!`/`option_env!` 依赖（同 L2：(名, 编译时值；None = 编译时未设)）
    envs: Vec<(String, Option<String>)>,
    /// v0 恒 None（全量模块；delta+BASE 形态预留）
    base_key: Option<String>,
    target: String,
}

/// NATIVELIBS 节条目：自产库清单（路径 + 内容哈希 + role）。
#[derive(Serialize, Deserialize)]
struct NativeLibEntry {
    path: String,
    /// 0=static_archive 1=global_asm（bin/dep 同族，cache/global-asm 域）
    role: u8,
    fnv: u128,
}

/// RELOC 节：固定基要求 + 入口符号（entry 语义锚在 Module.entry，本字段供人读）。
#[derive(Serialize, Deserialize)]
struct Reloc {
    requires_fixed_base: bool,
    entry: Box<str>,
}

fn postcard_bytes<T: Serialize>(v: &T) -> Result<Vec<u8>, String> {
    postcard::to_stdvec(v).map_err(|e| format!("包节序列化失败: {e}"))
}

/// 打包入账（lower 刚完成、guest 未运行的洁净快照——与 L2 store 同一时机）。
/// 拒绝 = 文案（固定基缺失/输入无法盖戳/native 库无法盖戳），调用方响亮终止。
pub(crate) fn write_package(
    tcx: rustc_middle::ty::TyCtxt<'_>,
    rustc_args: &[String],
    module: &crate::vm::engine::ir::Module,
    out: &Path,
) -> Result<(), String> {
    // 固定基要求（L2 同契约：非固定基 = 快照内嵌地址跨进程无效，不产包）
    if !module.frozen.as_ref().is_some_and(|f| f.at_fixed_base()) {
        return Err("冻结区不在固定基址（并发抢占/ASLR 冲突）——重试打包".into());
    }
    if !module.entry_stub_sites.is_empty() && !module.entry_stubs.at_fixed_base() {
        return Err("条目 stub 域不在固定基址——重试打包".into());
    }
    let (stamps, envs) = crate::ircache::collect_input_stamps(tcx)
        .ok_or("有输入文件无法盖戳（消失/非常规）——宁不打包")?;
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
    // NATIVELIBS：逐一 fnv128 现场计算；role 判别（global-asm 域 = 自产汇编族）
    let ga_dir = crate::sysroot::cache_dir().join("global-asm");
    let ga_prefix = ga_dir.display().to_string();
    let mut libs = Vec::new();
    for p in &module.required_native_libs {
        let data = std::fs::read(&**p).map_err(|e| format!("自产库 `{p}` 读取失败: {e}"))?;
        libs.push(NativeLibEntry {
            path: p.to_string(),
            role: u8::from(p.starts_with(&ga_prefix)),
            fnv: hash128(&data),
        });
    }
    let module_bytes =
        postcard_bytes(&module).map_err(|e| format!("module 序列化失败: {e}"))?;

    let sections: Vec<(u32, Vec<u8>)> = vec![
        (TAG_META, postcard_bytes(&meta)?),
        (TAG_STAMPS, postcard_bytes(&stamps)?),
        (TAG_MODULE, module_bytes),
        (TAG_NATIVELIBS, postcard_bytes(&libs)?),
        (TAG_RELOC, postcard_bytes(&reloc)?),
    ];

    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&FMT_VER.to_le_bytes());
    let bid = env!("MIRVM_BUILD_ID").as_bytes();
    buf.extend_from_slice(&(bid.len() as u32).to_le_bytes());
    buf.extend_from_slice(bid);
    buf.extend_from_slice(&(sections.len() as u32).to_le_bytes());
    // 节表项 = tag(4) + off(8) + len(8) + hash(16) = 36 字节
    let mut off = (buf.len() + sections.len() * 36) as u64;
    let mut table = Vec::new();
    for (tag, data) in &sections {
        table.extend_from_slice(&tag.to_le_bytes());
        table.extend_from_slice(&off.to_le_bytes());
        table.extend_from_slice(&(data.len() as u64).to_le_bytes());
        table.extend_from_slice(&hash128(data).to_le_bytes());
        off += data.len() as u64;
    }
    buf.extend_from_slice(&table);
    for (_, data) in &sections {
        buf.extend_from_slice(data);
    }
    let whole = hash128(&buf);
    buf.extend_from_slice(&whole.to_le_bytes());

    // 原子发布（全仓同款：临时名写全再 rename）
    let dir = out.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("包目录创建失败: {e}"))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        out.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, &buf).map_err(|e| format!("包写入失败: {e}"))?;
    std::fs::rename(&tmp, out).map_err(|e| format!("包发布失败: {e}"))?;
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

/// 装载 + 全校验（refuse-loud）。`MIRVM_PACK_NOSTAMP=1` 旁路输入戳
/// （分发到无源环境时使用；本机默认仍校验）。
pub(crate) fn load_package(path: &Path) -> Result<LoadedPackage, String> {
    let raw = std::fs::read(path).map_err(|e| format!("包读取失败: {e}"))?;
    if raw.len() < 8 + 4 * 3 + 16 || &raw[..8] != MAGIC {
        return Err("非 .mirvm 包（magic 不符）".into());
    }
    let (body, whole) = raw.split_at(raw.len() - 16);
    let mut got = [0u8; 16];
    got.copy_from_slice(whole);
    if u128::from_le_bytes(got) != hash128(body) {
        return Err("包全文件哈希不符（损坏或截断）".into());
    }
    let mut cur = 8usize;
    let u32_at = |cur: &mut usize, raw: &[u8]| -> u32 {
        let v = u32::from_le_bytes(raw[*cur..*cur + 4].try_into().unwrap());
        *cur += 4;
        v
    };
    let u64_at = |cur: &mut usize, raw: &[u8]| -> u64 {
        let v = u64::from_le_bytes(raw[*cur..*cur + 8].try_into().unwrap());
        *cur += 8;
        v
    };
    if u32_at(&mut cur, body) != FMT_VER {
        return Err("包格式版本不符——以同代 mirvm 重打或换对应构建".into());
    }
    let bid_len = u32_at(&mut cur, body) as usize;
    let bid = std::str::from_utf8(&body[cur..cur + bid_len])
        .map_err(|e| format!("包 build_id 非 UTF-8: {e}"))?;
    cur += bid_len;
    if bid != env!("MIRVM_BUILD_ID") {
        return Err("包 build_id 与当前 mirvm 不符（格式 v0 不跨构建；以当前 mirvm 重打）".into());
    }
    let cnt = u32_at(&mut cur, body) as usize;
    let mut metas: Vec<(u32, u64, u64, u128)> = Vec::with_capacity(cnt);
    for _ in 0..cnt {
        let tag = u32_at(&mut cur, body);
        let off = u64_at(&mut cur, body);
        let len = u64_at(&mut cur, body);
        let h = u128::from_le_bytes(body[cur..cur + 16].try_into().unwrap());
        cur += 16;
        metas.push((tag, off, len, h));
    }
    let section = |tag: u32| -> Result<&[u8], String> {
        let (_, off, len, h) = metas
            .iter()
            .find(|(t, _, _, _)| *t == tag)
            .ok_or_else(|| format!("包缺必需节 tag={tag}"))?;
        let (o, l) = (*off as usize, *len as usize);
        let data = body
            .get(o..o + l)
            .ok_or_else(|| format!("包节 tag={tag} 越界"))?;
        if hash128(data) != *h {
            return Err(format!("包节 tag={tag} 哈希不符"));
        }
        Ok(data)
    };
    let meta: Meta = postcard::from_bytes(section(TAG_META)?)
        .map_err(|e| format!("META 节解析失败: {e}"))?;
    if !crate::ircache::envs_current(&meta.envs) {
        return Err("包 env 依赖与当前环境不符（编译时 env! 值已变——以当前环境重打）".into());
    }
    if std::env::var_os("MIRVM_PACK_NOSTAMP").is_none() {
        let stamps: Vec<crate::ircache::FileStamp> = postcard::from_bytes(section(TAG_STAMPS)?)
            .map_err(|e| format!("STAMPS 节解析失败: {e}"))?;
        if !crate::ircache::stamps_current(&stamps) {
            let m = crate::ircache::stamps_first_mismatch(&stamps)
                .map(|f| f.path)
                .unwrap_or_default();
            return Err(format!(
                "包输入盖戳与本地文件不符（{m}——mirvm pack 重打；或 MIRVM_PACK_NOSTAMP=1 旁路）"
            ));
        }
    }
    let libs: Vec<NativeLibEntry> = postcard::from_bytes(section(TAG_NATIVELIBS)?)
        .map_err(|e| format!("NATIVELIBS 节解析失败: {e}"))?;
    for l in &libs {
        let data = std::fs::read(&l.path)
            .map_err(|e| format!("自产库 `{}` 缺失（{e}）——重打或恢复该缓存产物", l.path))?;
        if hash128(&data) != l.fnv {
            return Err(format!("自产库 `{}` 内容哈希不符（已变——重打）", l.path));
        }
    }
    let reloc: Reloc = postcard::from_bytes(section(TAG_RELOC)?)
        .map_err(|e| format!("RELOC 节解析失败: {e}"))?;
    let module: crate::vm::engine::ir::Module = postcard::from_bytes(section(TAG_MODULE)?)
        .map_err(|e| format!("MODULE 节解析失败（冻结区固定基恢复未成立？）: {e}"))?;
    if reloc.requires_fixed_base && !module.frozen.as_ref().is_some_and(|f| f.at_fixed_base()) {
        return Err("包要求固定基址但当前进程不可用（被占/ASLR 冲突）——重试或空闲后跑".into());
    }
    Ok(LoadedPackage { module })
}
