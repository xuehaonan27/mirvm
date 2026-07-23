//! MC 机器码节的进程内装载（mode B 片③，designs/modeb-mirvmar-design.md §5）：
//! 自产 `.so`（global_asm/dep_asm 族）**不经 dlopen**——自解析 ELF64、自映射、
//! 自重定位、注册 eh_frame、建符号表，并入 foreign 解析链的 ① 位（先于
//! RTLD_DEFAULT，与归档句柄同一语义位：guest 自产对象恒胜宿主同名库）。
//!
//! 数据源 = 包内 MC 节的 `.so` 原始字节（片② NATIVELIBS 的 fnv 互证）；本
//! 装载器对系统链接器零依赖（kernel mmap/mprotect + 自解析，无 ld.so/ld.so.cache
//! 概念）。边界（一律响亮拒绝）：非 ET_DYN x86_64、PT_INTERP、TLS/COPY 重定位、
//! 非弱未定义外部符号、STT_GNU_IFUNC——这些形态不属于自产 global_asm 族，
//! 遇到即说明该 .so 并非本族产物。

use std::collections::HashMap;

/// 装载完成的 MC 镜像（随进程生命周期——与 dlopen 句柄同一纪律，不卸）。
pub struct McImage {
    base: usize,
    #[allow(dead_code)] // 诊断面保留（调试打印用）
    size: usize,
    /// 符号 → 镜像内真地址（STB_GLOBAL/WEAK 且已定义；hidden 与 dynsym 两族并集）
    pub symbols: HashMap<Box<str>, u64>,
}

/// 进程级 MC 符号注册表（(基址, 符号表) 序 = 装载序，与 required_native_libs
/// 链接序同构）。resolve 的 ① 位在其上按序查。
static MC_REGISTRY: std::sync::RwLock<Vec<(u64, HashMap<Box<str>, u64>)>> =
    std::sync::RwLock::new(Vec::new());

/// 注册镜像（装载序追加；永不移除——与 dlopen 句柄同生命周期纪律）。
pub fn register(img: McImage) {
    let mut reg = MC_REGISTRY.write().unwrap();
    let bias = img.base as u64;
    reg.push((bias, img.symbols));
}

/// MC 符号解析（① 位语义：先于全域）。None = 全部 MC 镜像都没有该符号。
pub fn resolve(name: &str) -> Option<usize> {
    let reg = MC_REGISTRY.read().unwrap();
    for (bias, syms) in reg.iter() {
        if let Some(&v) = syms.get(name) {
            return Some((bias + v) as usize);
        }
    }
    None
}

// ===== ELF64 装载 =====

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

struct Shdr {
    name_off: u32,
    ty: u32,
    addr: u64,
    off: u64,
    size: u64,
    entsize: u64,
}

/// 装载一个 ELF64 DYN 镜像（自产 global_asm/dep_asm 族 .so 的原始字节）。
pub fn load(bytes: &[u8]) -> Result<McImage, String> {
    let bad = || "MC 镜像不是预期的 ELF64 LE DYN（或已损坏）".to_string();
    if bytes.len() < 64 || bytes[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(bad());
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        return Err(bad());
    }
    if u16_at(bytes, 16) != Some(3) {
        return Err("MC 镜像非 ET_DYN（自产族应是共享对象）".into());
    }
    if u16_at(bytes, 18) != Some(62) {
        return Err("MC 镜像非 x86_64（EM_X86_64）".into());
    }
    let phoff = u64_at(bytes, 32).ok_or_else(bad)? as usize;
    let phentsize = u16_at(bytes, 54).ok_or_else(bad)? as usize;
    let phnum = u16_at(bytes, 56).ok_or_else(bad)? as usize;
    let shoff = u64_at(bytes, 40).ok_or_else(bad)? as usize;
    let shentsize = u16_at(bytes, 58).ok_or_else(bad)? as usize;
    let shnum = u16_at(bytes, 60).ok_or_else(bad)? as usize;
    let shstrndx = u16_at(bytes, 62).ok_or_else(bad)? as usize;
    if phentsize < 56 || shentsize < 64 {
        return Err(bad());
    }
    let phdr = |i: usize| -> Option<(u32, u64, u64, u64, u64, u32, u64)> {
        // (type, off, vaddr, filesz, memsz, flags, align)
        let b = phoff.checked_add(i.checked_mul(phentsize)?)?;
        Some((
            u32_at(bytes, b)?,
            u64_at(bytes, b + 8)?,
            u64_at(bytes, b + 16)?,
            u64_at(bytes, b + 32)?,
            u64_at(bytes, b + 40)?,
            u32_at(bytes, b + 4)?,
            u64_at(bytes, b + 48)?,
        ))
    };
    let shdr = |i: usize| -> Option<Shdr> {
        let b = shoff.checked_add(i.checked_mul(shentsize)?)?;
        Some(Shdr {
            name_off: u32_at(bytes, b)?,
            ty: u32_at(bytes, b + 4)?,
            addr: u64_at(bytes, b + 16)?,
            off: u64_at(bytes, b + 24)?,
            size: u64_at(bytes, b + 32)?,
            entsize: u64_at(bytes, b + 56)?,
        })
    };

    // PT_LOAD 全景：span 计算 + PT_INTERP 拒（共享对象不应有）
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const PT_INTERP: u32 = 3;
    let page = 4096usize;
    let mut lo = usize::MAX;
    let mut hi = 0usize;
    let mut loads = Vec::new();
    let mut dyn_off = 0u64;
    for i in 0..phnum {
        let (ty, off, vaddr, filesz, memsz, _flags, _align) = phdr(i).ok_or_else(bad)?;
        match ty {
            PT_LOAD => {
                let v = vaddr as usize;
                let f = filesz as usize;
                let m = memsz as usize;
                if m < f || off as usize + f > bytes.len() {
                    return Err(bad());
                }
                loads.push((off, v, f, m));
                lo = lo.min(v & !(page - 1));
                hi = hi.max((v + m + page - 1) & !(page - 1));
            }
            PT_DYNAMIC => dyn_off = off,
            PT_INTERP => return Err("MC 镜像带 PT_INTERP（不是自产共享对象）".into()),
            _ => {}
        }
    }
    if loads.is_empty() || lo >= hi {
        return Err("MC 镜像无 PT_LOAD".into());
    }
    let size = hi - lo;
    // 段 flags（ELF：X=1 W=2 R=4）→ 最终 mprotect 形态
    let seg_prot = |flags: u32| -> crate::os::mem::Prot {
        if flags & 1 != 0 {
            crate::os::mem::Prot::RX
        } else {
            crate::os::mem::Prot::RW
        }
    };
    let raw = crate::os::mem::map_anon(size, crate::os::mem::Prot::RW, false);
    if raw.is_null() {
        return Err(format!("MC 镜像映射失败（{size:#x} 字节）"));
    }
    let base = raw as usize;
    // 拷贝 + BSS 清零 + 最终保护（按 flags；W=2 的段留在 RW，X=1 的段封 RX）
    for (i, (off, vaddr, filesz, memsz)) in loads.iter().enumerate() {
        let dst = base + (vaddr - lo);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr().add(*off as usize), dst as *mut u8, *filesz);
            if memsz > filesz {
                std::ptr::write_bytes((dst + filesz) as *mut u8, 0, memsz - filesz);
            }
        }
        let (ty, _, _, _, _, flags, _) = phdr(i).ok_or_else(bad)?;
        debug_assert_eq!(ty, PT_LOAD);
        let start = (dst) & !(page - 1);
        let end = (dst + memsz + page - 1) & !(page - 1);
        crate::os::mem::protect(start as *mut u8, end - start, seg_prot(flags))
            .map_err(|e| format!("MC 镜像段保护失败: {e}"))?;
    }

    // 节头串表（找 .symtab/.strtab/.eh_frame 用）
    let shstr = shdr(shstrndx).ok_or_else(bad)?;
    let sec_name = |s: &Shdr| -> &str {
        let start = (shstr.off + u64::from(s.name_off)) as usize;
        let end = bytes[start..]
            .iter()
            .position(|&c| c == 0)
            .map(|p| start + p)
            .unwrap_or(start);
        std::str::from_utf8(&bytes[start..end]).unwrap_or("")
    };

    // .dynamic：RELA/JMPREL 位置（vaddr → base 换算）
    let mut rela = None;
    let mut relasz = 0u64;
    let mut jmprel = None;
    let mut pltrelsz = 0u64;
    if dyn_off != 0 {
        let mut d = base + (dyn_off as usize - lo);
        loop {
            let tag = i64::from_le_bytes(unsafe { std::ptr::read(d as *const [u8; 8]) });
            let val = u64::from_le_bytes(unsafe { std::ptr::read((d + 8) as *const [u8; 8]) });
            d += 16;
            match tag {
                0 => break,
                7 => rela = Some(val),  // DT_RELA
                8 => relasz = val,      // DT_RELASZ
                23 => jmprel = Some(val), // DT_JMPREL
                2 => pltrelsz = val,    // DT_PLTRELSZ
                _ => {}
            }
        }
    }

    // .symtab/.strtab/.eh_frame 收集
    let mut symtab: Option<Shdr> = None;
    let mut strtab: Option<Shdr> = None;
    let mut eh_frame: Option<Shdr> = None;
    for i in 0..shnum {
        let s = shdr(i).ok_or_else(bad)?;
        match (s.ty, sec_name(&s)) {
            (2, _) => symtab = Some(s),                 // SHT_SYMTAB
            (3, ".strtab") => strtab = Some(s),         // SHT_STRTAB
            (_, ".eh_frame") => eh_frame = Some(s),
            _ => {}
        }
    }
    let (sym_s, str_s) = (symtab.ok_or("MC 镜像缺 .symtab")?, strtab.ok_or("MC 镜像缺 .strtab")?);
    let str_at = |off: u32| -> Result<String, String> {
        let start = (str_s.off + u64::from(off)) as usize;
        let limit = (str_s.off + str_s.size) as usize;
        let end = bytes[start..limit.min(bytes.len())]
            .iter()
            .position(|&c| c == 0)
            .map(|p| start + p)
            .ok_or("MC 镜像 strtab 越界")?;
        Ok(std::str::from_utf8(&bytes[start..end])
            .map_err(|_| "MC 镜像符号名非 UTF-8")?
            .to_string())
    };
    let syment = sym_s.entsize.max(24) as usize;
    let symcount = (sym_s.size as usize) / syment;
    let sym_at = |j: usize| -> Option<(u32, u8, u16, u64)> {
        // (st_name, st_info, st_shndx, st_value)
        let b = (sym_s.off as usize).checked_add(j.checked_mul(syment)?)?;
        Some((
            u32_at(bytes, b)?,
            bytes.get(b + 4).copied()?,
            u16_at(bytes, b + 6)?,
            u64_at(bytes, b + 8)?,
        ))
    };

    // 符号表（注册面：GLOBAL/WEAK 且已定义；hidden/dynsym 两族并集）
    let mut symbols: HashMap<Box<str>, u64> = HashMap::new();
    for j in 0..symcount {
        let (name_off, info, shndx, value) = sym_at(j).ok_or_else(bad)?;
        let bind = info >> 4;
        if name_off == 0 || shndx == 0 || shndx >= 0xff00 || (bind != 1 && bind != 2) {
            continue;
        }
        let name = str_at(name_off)?;
        // 存 vaddr（register/resolve 统一 bias+value，与 archive_fallbacks 同形）
        symbols.insert(name.into_boxed_str(), value);
    }

    // 重定位应用
    let apply = |off: u64, info: u64, addend: i64| -> Result<(), String> {
        let ty = info as u32;
        let sym_idx = (info >> 32) as usize;
        let place = (base as u64).wrapping_add(off) as usize;
        // 符号地址：内部（自 symtab 已定义）→ 外部（RTLD_DEFAULT）→ 弱缺席 0
        let sym_addr = |idx: usize| -> Result<u64, String> {
            if idx == 0 {
                return Ok(0);
            }
            let (name_off, info2, shndx, value) = sym_at(idx).ok_or_else(bad)?;
            if shndx != 0 && shndx < 0xff00 {
                return Ok((base as u64) + value);
            }
            let name = str_at(name_off)?;
            let c = std::ffi::CString::new(name.as_str()).map_err(|_| "符号名含 NUL")?;
            let p = crate::os::dll::sym(0, &c);
            if p != 0 {
                return Ok(p as u64);
            }
            if (info2 >> 4) == 2 {
                return Ok(0); // WEAK 缺席 = 0
            }
            Err(format!("MC 重定位符号 `{name}` 未命中（RTLD_DEFAULT 均无）"))
        };
        match ty {
            0 => Ok(()), // NONE
            8 => {
                // RELATIVE：*(place) = base + addend
                unsafe { std::ptr::write_unaligned(place as *mut u64, (base as u64).wrapping_add(addend as u64)) };
                Ok(())
            }
            1 => {
                // 64：*(place) = sym + addend
                let s = sym_addr(sym_idx)?;
                unsafe { std::ptr::write_unaligned(place as *mut u64, s.wrapping_add(addend as u64)) };
                Ok(())
            }
            2 => {
                // PC32：*(place) = sym + addend - place
                let s = sym_addr(sym_idx)?;
                let v = (s as i64 + addend - place as i64) as u32;
                unsafe { std::ptr::write_unaligned(place as *mut u32, v) };
                Ok(())
            }
            6 | 7 => {
                // GLOB_DAT / JUMP_SLOT：*(place) = sym
                let s = sym_addr(sym_idx)?;
                unsafe { std::ptr::write_unaligned(place as *mut u64, s) };
                Ok(())
            }
            16 | 17 | 18 => Err("MC 镜像含 TLS 重定位（DTPMOD/DTPOFF 未接）".into()),
            5 => Err("MC 镜像含 COPY 重定位（不接）".into()),
            other => Err(format!("MC 镜像含未支持重定位类型 {other}")),
        }
    };
    if let Some(r0) = rela {
        let cnt = (relasz / 24) as usize;
        for i in 0..cnt {
            let b = (base + (r0 as usize - lo)) + i * 24;
            let (off, info, addend) = unsafe {
                (
                    std::ptr::read_unaligned(b as *const u64),
                    std::ptr::read_unaligned((b + 8) as *const u64),
                    std::ptr::read_unaligned((b + 16) as *const i64),
                )
            };
            apply(off, info, addend)?;
        }
    }
    if let Some(j0) = jmprel {
        let cnt = (pltrelsz / 24) as usize;
        for i in 0..cnt {
            let b = (base + (j0 as usize - lo)) + i * 24;
            let (off, info, addend) = unsafe {
                (
                    std::ptr::read_unaligned(b as *const u64),
                    std::ptr::read_unaligned((b + 8) as *const u64),
                    std::ptr::read_unaligned((b + 16) as *const i64),
                )
            };
            apply(off, info, addend)?;
        }
    }

    // eh_frame 注册（与 JIT 同一 __register_frame 语义；FDE 地址已随 base 就位）
    if let Some(eh) = eh_frame {
        unsafe extern "C" {
            fn __register_frame(fde: *const u8);
        }
        let start = base + (eh.addr as usize - lo);
        let end = start + eh.size as usize;
        let mut cur = start;
        while cur + 8 <= end {
            let len = u32::from_le_bytes(unsafe { std::ptr::read(cur as *const [u8; 4]) }) as usize;
            if len == 0 {
                break;
            }
            let cie_ptr = u32::from_le_bytes(unsafe { std::ptr::read((cur + 4) as *const [u8; 4]) });
            if cie_ptr != 0 {
                unsafe { __register_frame(cur as *const u8) };
            }
            cur += len + 4;
        }
    }

    Ok(McImage { base, size, symbols })
}
