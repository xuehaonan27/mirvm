//! 静态归档 `.so` 的 `.symtab` 兜底符号解析（M5.1 归档装载补片）。
//!
//! `-fvisibility=hidden` 编译的归档（ring 的 build.rs 显式加该旗标、zstd-sys
//! 同族）经 `-shared --whole-archive` 转换后，符号被 localize、**不进 .dynsym**
//! ——dlsym 全域/按句柄均未命中，但 `.symtab` 完整保留（含本地化后的地址）。
//!
//! 解析序（FfiState 直调 / fn-ptr 取址 / extern static 三处一致）：**hidden
//! 符号（本模块兜底表）先于 dlsym(RTLD_DEFAULT)**。这是 native 链接期绑定语义
//! 的直译：静态归档成员链进 guest 二进制后，guest 引用恒绑定归档内定义，盖过
//! 全局命名空间——宿主进程经 librustc_driver 装载的 libLLVM 内嵌并导出全套
//! `ZSTD_*`（`@@LLVM_22.1` 版本符号），dlsym 优先会让 guest 的 zstd 调用静默
//! 绑到宿主库（corpus c_zstd_stream 实锤：合法但不同的压缩字节）。dynsym 可见
//! 符号不进兜底表：它们维持 dlsym 全域解析（reject_symbol_ambiguity 物化期已
//! 拒全局碰撞，且 IFUNC 等 dlsym 语义只在动态面成立）。
//!
//! 只用于 mirvm 自己物化的归档 `.so`（required_native_libs）：格式由
//! native_archive 的受约束链接产出（ELF64 LE x86_64，非 strip）；系统库
//! 一律有正常 .dynsym，不走此道。
//!
//! dlopen 句柄 → 装载基址（dlinfo）在 `os::dll::load_bias`（P7 os 层）。

use std::collections::HashMap;

const SHT_DYNSYM: u32 = 11;
const SHT_SYMTAB: u32 = 2;

/// 解析 `.so` 的 `.symtab`：已定义符号名 → st_value（文件虚拟地址，相对装载基址）。
/// Err = 不是预期的 ELF64 LE / 结构越界（格式损坏——物化产物不应如此）。
/// 产品面只用 hidden_symtab_values；本原始视图留作单测对照（visible 符号双道同址）。
#[cfg(test)]
pub(crate) fn symtab_values(so_path: &str) -> Result<HashMap<Box<str>, u64>, String> {
    symbol_table_values(so_path, SHT_SYMTAB)
}

/// hidden 符号兜底表 = `.symtab` 已定义 − `.dynsym` 已定义。
/// dynsym 可见符号维持 dlsym 全域解析（物化期 reject_symbol_ambiguity 已拒其与
/// RTLD_DEFAULT 的碰撞）；只有 dlsym 不可达的 hidden 符号走"归档优先于全局"
/// 的链接期绑定语义（见模块头注）。解析失败按 Err 上抛，调用方一律退化为无表。
pub fn hidden_symtab_values(so_path: &str) -> Result<HashMap<Box<str>, u64>, String> {
    let mut syms = symbol_table_values(so_path, SHT_SYMTAB)?;
    for name in symbol_table_values(so_path, SHT_DYNSYM)?.keys() {
        syms.remove(&**name);
    }
    Ok(syms)
}

/// 解析指定符号表节（SHT_SYMTAB / SHT_DYNSYM，条目格式相同）：
/// 已定义符号名 → st_value（文件虚拟地址，相对装载基址）。
fn symbol_table_values(so_path: &str, want_sht: u32) -> Result<HashMap<Box<str>, u64>, String> {
    let bytes =
        std::fs::read(so_path).map_err(|e| format!("读取归档共享库 `{so_path}` 失败: {e}"))?;
    let u16_at = |off: usize| -> Option<u16> {
        Some(u16::from_le_bytes(
            bytes.get(off..off + 2)?.try_into().ok()?,
        ))
    };
    let u32_at = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(off..off + 4)?.try_into().ok()?,
        ))
    };
    let u64_at = |off: usize| -> Option<u64> {
        Some(u64::from_le_bytes(
            bytes.get(off..off + 8)?.try_into().ok()?,
        ))
    };
    let bad = || format!("归档共享库 `{so_path}` 不是预期的 ELF64 LE（或已损坏）");
    if bytes.len() < 64 || bytes[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(bad());
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        // EI_CLASS=ELFCLASS64, EI_DATA=ELFDATA2LSB
        return Err(bad());
    }
    let shoff = u64_at(0x28).ok_or_else(bad)? as usize;
    let shentsize = u16_at(0x3a).ok_or_else(bad)? as usize;
    let mut shnum = u16_at(0x3c).ok_or_else(bad)? as usize;
    if shentsize < 64 {
        return Err(bad());
    }
    let shdr = |i: usize| -> Option<(u32, u64, u64, u32, u64)> {
        // (sh_type, sh_offset, sh_size, sh_link, sh_entsize)
        let base = shoff.checked_add(i.checked_mul(shentsize)?)?;
        Some((
            u32_at(base + 4)?,
            u64_at(base + 24)?,
            u64_at(base + 32)?,
            u32_at(base + 40)?,
            u64_at(base + 56)?,
        ))
    };
    if shnum == 0 {
        // SHN_UNDEF 扩展：真实节数在 shdr[0].sh_size
        let (_, _, size, _, _) = shdr(0).ok_or_else(bad)?;
        shnum = usize::try_from(size).map_err(|_| bad())?;
    }
    for i in 0..shnum {
        let (ty, sym_off, sym_size, link, entsize) = shdr(i).ok_or_else(bad)?;
        if ty != want_sht {
            continue;
        }
        if entsize < 24 {
            return Err(bad());
        }
        let str_idx = usize::try_from(link).map_err(|_| bad())?;
        let (_, str_off, str_size, _, _) = shdr(str_idx).ok_or_else(bad)?;
        let str_end = usize::try_from(str_off + str_size).map_err(|_| bad())?;
        let mut out = HashMap::new();
        let count = usize::try_from(sym_size / entsize.max(1)).map_err(|_| bad())?;
        for j in 0..count {
            let base = usize::try_from(sym_off)
                .ok()
                .and_then(|o| o.checked_add(j.checked_mul(entsize as usize)?))
                .ok_or_else(bad)?;
            let st_name = u32_at(base).ok_or_else(bad)? as usize;
            let st_shndx = u16_at(base + 6).ok_or_else(bad)?;
            let st_value = u64_at(base + 8).ok_or_else(bad)?;
            // SHN_UNDEF(0) 与保留段索引（0xff00+）不取
            if st_name == 0 || st_shndx == 0 || st_shndx >= 0xff00 {
                continue;
            }
            let name_start = usize::try_from(str_off)
                .ok()
                .and_then(|o| o.checked_add(st_name))
                .ok_or_else(bad)?;
            let name_end = bytes[name_start..str_end.min(bytes.len())]
                .iter()
                .position(|&b| b == 0)
                .map(|p| name_start + p)
                .ok_or_else(bad)?;
            let name = std::str::from_utf8(&bytes[name_start..name_end]).map_err(|_| bad())?;
            out.insert(Box::from(name), st_value);
        }
        return Ok(out);
    }
    // 无所求符号表节（strip 过的物化产物不应出现，但无害）：按空表处理
    Ok(HashMap::new())
}

// ===== C2：ar 归档的 SHN_UNDEF 静态枚举（native-archive 闭包「符号在 rlib」判定面）=====

/// Unix ar 归档的未定义符号静态枚举（C2，2026-07-18）：逐成员遍历（跳过 ar 符号
/// 表/长名表成员），对每个 ELF64 成员读 `.symtab`，收集 `SHN_UNDEF` 的
/// GLOBAL/WEAK 绑定符号名。二进制级解析（与 `symbol_table_values` 同款结构走法），
/// 不解析任何工具文本输出。
pub fn archive_undefined_symbols(archive_path: &str) -> Result<Vec<Box<str>>, String> {
    let bytes = std::fs::read(archive_path)
        .map_err(|e| format!("读取静态原生归档 `{archive_path}` 失败: {e}"))?;
    archive_undefined_symbols_in(&bytes)
        .map_err(|why| format!("静态原生归档 `{archive_path}` 未定义符号枚举失败: {why}"))
}

/// 字节面实现（成员头表链 = 60B 定长：name[16] date[12] uid[6] gid[6] mode[8]
/// size[10] "`\n"；成员体按 size 对齐 2）。
fn archive_undefined_symbols_in(bytes: &[u8]) -> Result<Vec<Box<str>>, String> {
    if !bytes.starts_with(b"!<arch>\n") {
        return Err("不是 Unix ar 归档".into());
    }
    let mut out: Vec<Box<str>> = Vec::new();
    let mut pos = 8usize;
    while pos + 60 <= bytes.len() {
        let hdr = &bytes[pos..pos + 60];
        if &hdr[58..60] != b"`\n" {
            return Err(format!("ar 成员头表魔数错位 @{pos:#x}"));
        }
        let size_txt = std::str::from_utf8(&hdr[48..58]).map_err(|_| "ar 成员尺寸非 ASCII")?;
        let size: usize = size_txt
            .trim()
            .parse()
            .map_err(|_| format!("ar 成员尺寸不可解析 `{size_txt}`"))?;
        let body_end = pos + 60 + size;
        if body_end > bytes.len() {
            return Err("ar 成员体越界".into());
        }
        // ar 成员元数据判定必须精确（C2 施工实锤）：GNU ar 对 >15 字符成员名
        // 用字符串表引用 `/N`（如 `/0`）——以 '/' 开头**不等于**元数据；
        // 真正的元数据只有符号表（`/`、`__.SYMDEF`、`/SYM64/`）与字符串表（`//`）。
        let name = String::from_utf8_lossy(&hdr[0..16]);
        let name = name.trim();
        let is_metadata = name == "/"
            || name == "//"
            || name == "__.SYMDEF"
            || name == "__.SYMDEF SORTED"
            || name == "/SYM64/";
        if !is_metadata {
            let mut body = &bytes[pos + 60..body_end];
            // BSD 风格 `/#1/<len>`：名字内嵌体首，先剥名字长才是内容
            if let Some(rest) = name.strip_prefix("/#1/")
                && let Ok(nlen) = rest.trim().parse::<usize>()
            {
                body = &body[nlen.min(body.len())..];
            }
            if body.starts_with(b"\x7fELF") {
                for sym in elf_undefined_symbols(body)? {
                    if !out.contains(&sym) {
                        out.push(sym);
                    }
                }
            }
            // 非 ELF 成员（文本清单等）：跳过
        }
        pos = body_end + (size & 1);
    }
    Ok(out)
}

/// 单 ELF64 LE 字节面的 SHN_UNDEF 枚举（GLOBAL/WEAK 绑定；与
/// symbol_table_values 同款结构走法，只是取 shndx==0 且不过滤）。
fn elf_undefined_symbols(bytes: &[u8]) -> Result<Vec<Box<str>>, String> {
    let u16_at = |off: usize| -> Option<u16> {
        Some(u16::from_le_bytes(
            bytes.get(off..off + 2)?.try_into().ok()?,
        ))
    };
    let u32_at = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(off..off + 4)?.try_into().ok()?,
        ))
    };
    let u64_at = |off: usize| -> Option<u64> {
        Some(u64::from_le_bytes(
            bytes.get(off..off + 8)?.try_into().ok()?,
        ))
    };
    let bad = || "不是预期的 ELF64 LE（或已损坏）".to_string();
    if bytes.len() < 64 || bytes[0..4] != [0x7f, b'E', b'L', b'F'] || bytes[4] != 2 || bytes[5] != 1
    {
        return Err(bad());
    }
    let shoff = u64_at(0x28).ok_or_else(bad)? as usize;
    let shentsize = u16_at(0x3a).ok_or_else(bad)? as usize;
    let mut shnum = u16_at(0x3c).ok_or_else(bad)? as usize;
    if shentsize < 64 {
        return Err(bad());
    }
    let shdr = |i: usize| -> Option<(u32, u64, u64, u32, u64)> {
        let base = shoff.checked_add(i.checked_mul(shentsize)?)?;
        Some((
            u32_at(base + 4)?,
            u64_at(base + 24)?,
            u64_at(base + 32)?,
            u32_at(base + 40)?,
            u64_at(base + 56)?,
        ))
    };
    if shnum == 0 {
        let (_, _, size, _, _) = shdr(0).ok_or_else(bad)?;
        shnum = usize::try_from(size).map_err(|_| bad())?;
    }
    for i in 0..shnum {
        let (ty, sym_off, sym_size, link, entsize) = shdr(i).ok_or_else(bad)?;
        if ty != SHT_SYMTAB {
            continue;
        }
        if entsize < 24 {
            return Err(bad());
        }
        let str_idx = usize::try_from(link).map_err(|_| bad())?;
        let (_, str_off, str_size, _, _) = shdr(str_idx).ok_or_else(bad)?;
        let str_end = usize::try_from(str_off + str_size).map_err(|_| bad())?;
        let count = usize::try_from(sym_size / entsize.max(1)).map_err(|_| bad())?;
        let mut out = Vec::new();
        for j in 0..count {
            let base = usize::try_from(sym_off)
                .ok()
                .and_then(|o| o.checked_add(j.checked_mul(entsize as usize)?))
                .ok_or_else(bad)?;
            let st_name = u32_at(base).ok_or_else(bad)? as usize;
            let st_info = bytes.get(base + 4).copied().ok_or_else(bad)?;
            let st_shndx = u16_at(base + 6).ok_or_else(bad)?;
            let bind = st_info >> 4;
            // 只取未定义（SHN_UNDEF）的全局/弱绑定符号（LOCAL 是成员内部事）
            if st_name == 0 || st_shndx != 0 || (bind != 1 && bind != 2) {
                continue;
            }
            let name_start = usize::try_from(str_off)
                .ok()
                .and_then(|o| o.checked_add(st_name))
                .ok_or_else(bad)?;
            let name_end = bytes[name_start..str_end.min(bytes.len())]
                .iter()
                .position(|&b| b == 0)
                .map(|p| name_start + p)
                .ok_or_else(bad)?;
            let name = std::str::from_utf8(&bytes[name_start..name_end]).map_err(|_| bad())?;
            out.push(Box::from(name));
        }
        return Ok(out);
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::{archive_undefined_symbols, hidden_symtab_values, symtab_values};
    use std::ffi::CString;
    use std::process::Command;

    /// C2：ar 归档的 SHN_UNDEF 静态枚举（定义与未定义并存、含 LOCAL 与
    /// ar 元数据成员时只报 GLOBAL/WEAK 未定义）。
    #[test]
    fn archive_undefined_symbols_reports_global_and_weak_undef_only() {
        let dir = std::env::temp_dir().join(format!("mirvm-elfsym-arundef-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (c1, o1, c2, o2, a) = (
            dir.join("a.c"),
            // 成员名 >15 字符：强制 GNU ar 走字符串表引用（/N）形态
            dir.join("very-long-member-name-a.o"),
            dir.join("b.c"),
            dir.join("b.o"),
            dir.join("libp.a"),
        );
        // a.o：未定义引用 rlib 侧符号（GLOBAL）+ 弱未定义 + 自含定义（global 与
        // static 各一）——「used but never defined」的 static 在本 cc 下也发
        // GLOBAL undef（nm 实证），不作为 LOCAL 过滤面；LOCAL 过滤用静态定义验
        std::fs::write(
            &c1,
            "extern int rlib_side_def(long);\n\
             __attribute__((weak)) extern int weak_missing(void);\n\
             int defined_here(void) { return 1; }\n\
             static int static_defined(void) { return 2; }\n\
             int tramp(long x) { return rlib_side_def(x) + weak_missing() + defined_here() + static_defined(); }\n",
        )
        .unwrap();
        // b.o：全定义无未定义
        std::fs::write(&c2, "int other(void) { return 2; }\n").unwrap();
        for (c, o) in [(&c1, &o1), (&c2, &o2)] {
            assert!(
                Command::new("cc")
                    .args(["-fPIC", "-c"])
                    .arg(c)
                    .arg("-o")
                    .arg(o)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert!(
            Command::new("ar")
                .args(["crs"])
                .arg(&a)
                .arg(&o1)
                .arg(&o2)
                .status()
                .unwrap()
                .success()
        );
        let undef = archive_undefined_symbols(a.to_str().unwrap()).unwrap();
        assert!(
            undef.iter().any(|s| &**s == "rlib_side_def"),
            "GLOBAL 未定义漏报: {undef:?}"
        );
        assert!(
            undef.iter().any(|s| &**s == "weak_missing"),
            "WEAK 未定义漏报: {undef:?}"
        );
        assert!(
            !undef.iter().any(|s| &**s == "defined_here"),
            "已定义符号误报: {undef:?}"
        );
        assert!(
            !undef.iter().any(|s| &**s == "static_defined"),
            "static 已定义符号误报: {undef:?}"
        );
        assert!(
            !undef.iter().any(|s| &**s == "other"),
            "第二成员已定义符号误报: {undef:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 与 native_archive 同参数链接一个 -fvisibility=hidden 的归档：
    /// 符号不进 .dynsym，但 .symtab 兜底必须能解析出与 dlsym 直取一致的地址。
    #[test]
    fn hidden_symbols_resolve_via_symtab_with_same_address_as_dlsym() {
        let dir = std::env::temp_dir().join(format!("mirvm-elfsym-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (c, o, a, so) = (
            dir.join("p.c"),
            dir.join("p.o"),
            dir.join("libp.a"),
            dir.join("libp.so"),
        );
        std::fs::write(
            &c,
            "__attribute__((visibility(\"hidden\"))) unsigned long mirvm_hidden_probe(void) { return 0x2aUL; }\n\
             unsigned long mirvm_visible_probe(void) { return mirvm_hidden_probe(); }\n",
        )
        .unwrap();
        assert!(
            Command::new("cc")
                .args(["-fPIC", "-c",])
                .arg(&c)
                .arg("-o")
                .arg(&o)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("ar")
                .args(["crs"])
                .arg(&a)
                .arg(&o)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("cc")
                .args(["-shared", "-Wl,-z,defs", "-Wl,--whole-archive"])
                .arg(&a)
                .args(["-Wl,--no-whole-archive", "-o"])
                .arg(&so)
                .status()
                .unwrap()
                .success()
        );
        let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
        let handle = crate::os::dll::open_with_flags(
            &c_so,
            crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
        )
        .expect("dlopen hidden/visible probe .so");
        // hidden 符号 dlsym 未命中；visible 符号命中
        assert_eq!(crate::os::dll::sym(handle, c"mirvm_hidden_probe"), 0);
        let pvis = crate::os::dll::sym(handle, c"mirvm_visible_probe");
        assert!(pvis != 0);
        // .symtab 兜底：hidden 符号可解，且调用结果正确
        let syms = symtab_values(so.to_str().unwrap()).unwrap();
        let bias = crate::os::dll::load_bias(handle).expect("load_bias") as u64;
        let hidden_addr = *syms
            .get("mirvm_hidden_probe")
            .expect("symtab 含 hidden 符号")
            + bias;
        let f: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(hidden_addr as usize) };
        assert_eq!(unsafe { f() }, 0x2a);
        // visible 符号两条路径地址必须一致
        let vis_via_symtab = *syms
            .get("mirvm_visible_probe")
            .expect("symtab 含 visible 符号")
            + bias;
        assert_eq!(vis_via_symtab, pvis as u64);
        // hidden 兜底表 = .symtab − .dynsym：hidden 在表（归档优先的承载），
        // visible 出局（维持 dlsym 全域解析，与 reject_symbol_ambiguity 配套）
        let hidden_only = hidden_symtab_values(so.to_str().unwrap()).unwrap();
        assert_eq!(
            hidden_only.get("mirvm_hidden_probe"),
            syms.get("mirvm_hidden_probe"),
            "hidden 符号必须留在兜底表"
        );
        assert!(
            !hidden_only.contains_key("mirvm_visible_probe"),
            "dynsym 可见符号不进兜底表"
        );
        unsafe { crate::os::dll::close(handle) };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
