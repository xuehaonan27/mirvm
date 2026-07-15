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

use std::collections::HashMap;

/// glibc `struct link_map` 的首字段（ABI 恒久稳定；libc crate 未导出该类型）。
/// 只需 l_addr——ELF 文件内地址与内存地址的差值（装载基址）。
#[repr(C)]
struct LinkMap {
    l_addr: usize,
    l_name: *const std::ffi::c_char,
    l_ld: *mut std::ffi::c_void,
    l_next: *mut LinkMap,
    l_prev: *mut LinkMap,
}

/// dlopen 句柄的装载基址。None = dlinfo 失败（句柄非法——刚 dlopen 的句柄不应发生）。
pub fn load_bias(handle: *mut std::ffi::c_void) -> Option<u64> {
    let mut lm: *mut LinkMap = std::ptr::null_mut();
    let r = unsafe {
        libc::dlinfo(
            handle,
            libc::RTLD_DI_LINKMAP,
            &mut lm as *mut *mut LinkMap as *mut libc::c_void,
        )
    };
    if r == 0 && !lm.is_null() {
        Some(unsafe { (*lm).l_addr } as u64)
    } else {
        None
    }
}

const SHT_DYNSYM: u32 = 11;
const SHT_SYMTAB: u32 = 2;

/// 解析 `.so` 的 `.symtab`：已定义符号名 → st_value（文件虚拟地址，相对装载基址）。
/// Err = 不是预期的 ELF64 LE / 结构越界（格式损坏——物化产物不应如此）。
/// 产品面只用 hidden_symtab_values；本原始视图留作单测对照（visible 符号双道同址）。
#[cfg(test)]
pub fn symtab_values(so_path: &str) -> Result<HashMap<Box<str>, u64>, String> {
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
    let bytes = std::fs::read(so_path)
        .map_err(|e| format!("读取归档共享库 `{so_path}` 失败: {e}"))?;
    let u16_at = |off: usize| -> Option<u16> {
        Some(u16::from_le_bytes(bytes.get(off..off + 2)?.try_into().ok()?))
    };
    let u32_at = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(off..off + 4)?.try_into().ok()?))
    };
    let u64_at = |off: usize| -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(off..off + 8)?.try_into().ok()?))
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

#[cfg(test)]
mod tests {
    use super::{hidden_symtab_values, load_bias, symtab_values};
    use std::ffi::CString;
    use std::process::Command;

    /// 与 native_archive 同参数链接一个 -fvisibility=hidden 的归档：
    /// 符号不进 .dynsym，但 .symtab 兜底必须能解析出与 dlsym 直取一致的地址。
    #[test]
    fn hidden_symbols_resolve_via_symtab_with_same_address_as_dlsym() {
        let dir = std::env::temp_dir().join(format!(
            "mirvm-elfsym-test-{}",
            std::process::id()
        ));
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
        assert!(Command::new("cc")
            .args(["-fPIC", "-c", ])
            .arg(&c)
            .arg("-o")
            .arg(&o)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("ar")
            .args(["crs"])
            .arg(&a)
            .arg(&o)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("cc")
            .args(["-shared", "-Wl,-z,defs", "-Wl,--whole-archive"])
            .arg(&a)
            .args(["-Wl,--no-whole-archive", "-o"])
            .arg(&so)
            .status()
            .unwrap()
            .success());
        let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
        let handle = unsafe { libc::dlopen(c_so.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        assert!(!handle.is_null());
        // hidden 符号 dlsym 未命中；visible 符号命中
        let hidden = c"mirvm_hidden_probe";
        assert!(unsafe { libc::dlsym(handle, hidden.as_ptr()) }.is_null());
        let visible = c"mirvm_visible_probe";
        let pvis = unsafe { libc::dlsym(handle, visible.as_ptr()) };
        assert!(!pvis.is_null());
        // .symtab 兜底：hidden 符号可解，且调用结果正确
        let syms = symtab_values(so.to_str().unwrap()).unwrap();
        let bias = load_bias(handle).expect("load_bias");
        let hidden_addr = *syms.get("mirvm_hidden_probe").expect("symtab 含 hidden 符号") + bias;
        let f: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(hidden_addr as usize) };
        assert_eq!(unsafe { f() }, 0x2a);
        // visible 符号两条路径地址必须一致
        let vis_via_symtab = *syms.get("mirvm_visible_probe").expect("symtab 含 visible 符号") + bias;
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
        unsafe { libc::dlclose(handle) };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
