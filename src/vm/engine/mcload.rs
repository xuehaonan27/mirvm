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
use std::sync::atomic::{AtomicBool, Ordering};

/// 装载完成的 MC 镜像。符号只对持有它的 Module 可见；映射与 unwind 注册仍因
/// 外部代码指针可能存活而不卸载。
#[derive(Debug)]
pub struct McImage {
    mapping: usize,
    load_bias: usize,
    #[allow(dead_code)] // 诊断面保留（调试打印用）
    size: usize,
    /// 符号 → 镜像内真地址（STB_GLOBAL/WEAK 且已定义；hidden 与 dynsym 两族并集）
    pub symbols: HashMap<Box<str>, u64>,
    executable_ranges: Box<[(usize, usize)]>,
    lifecycle: super::native_instance::NativeLifecycle,
    registered_frames: Box<[usize]>,
    committed: AtomicBool,
}

/// MC 符号解析（① 位语义：先于全域）。镜像列表来自当前 Module，不能跨 Engine
/// 搜索，否则两个包里的同名 global_asm 会互相串线。
pub fn resolve(images: &[McImage], name: &str) -> Option<usize> {
    for image in images {
        if let Some(&v) = image.symbols.get(name) {
            return image.load_bias.checked_add(usize::try_from(v).ok()?);
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

#[derive(Clone, Copy)]
struct Shdr {
    name_off: u32,
    ty: u32,
    addr: u64,
    off: u64,
    size: u64,
    link: u32,
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
            link: u32_at(bytes, b + 40)?,
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
    let mut dynamic = None;
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
                loads.push((off, v, f, m, _flags));
                lo = lo.min(v & !(page - 1));
                hi = hi.max((v + m + page - 1) & !(page - 1));
            }
            PT_DYNAMIC => dynamic = Some((vaddr, memsz)),
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
    struct MappingGuard {
        mapping: *mut u8,
        size: usize,
        armed: bool,
    }
    impl Drop for MappingGuard {
        fn drop(&mut self) {
            if self.armed {
                unsafe { crate::os::mem::unmap(self.mapping, self.size) };
            }
        }
    }
    let mut mapping_guard = MappingGuard {
        mapping: raw,
        size,
        armed: true,
    };
    let mapping = raw as usize;
    let load_bias = mapping
        .checked_sub(lo)
        .ok_or("MC image mapping lies below its first ELF virtual address")?;
    let loaded_address = |vaddr: u64, what: &str| -> Result<usize, String> {
        load_bias
            .checked_add(
                usize::try_from(vaddr)
                    .map_err(|_| format!("MC {what} virtual address does not fit usize"))?,
            )
            .ok_or_else(|| format!("MC {what} virtual address overflow"))
    };
    let loaded_signed_address = |value: i64, what: &str| -> Result<u64, String> {
        let address = if value >= 0 {
            load_bias.checked_add(value as usize)
        } else {
            load_bias.checked_sub(value.unsigned_abs() as usize)
        }
        .ok_or_else(|| format!("MC {what} signed address overflow"))?;
        Ok(address as u64)
    };
    let range_in_load = |address: u64, range_size: u64| -> bool {
        let Some(end) = address.checked_add(range_size) else {
            return false;
        };
        loads.iter().any(|(_, vaddr, _, memsz, _)| {
            let start = *vaddr as u64;
            start
                .checked_add(*memsz as u64)
                .is_some_and(|load_end| address >= start && end <= load_end)
        })
    };
    // Keep the entire mapping writable through relocation. Final segment
    // protections are applied only after every relocation has been written.
    for (off, vaddr, filesz, memsz, _) in &loads {
        let dst = loaded_address(*vaddr as u64, "PT_LOAD")?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(*off as usize),
                dst as *mut u8,
                *filesz,
            );
            if memsz > filesz {
                std::ptr::write_bytes((dst + filesz) as *mut u8, 0, memsz - filesz);
            }
        }
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
    let mut init = None;
    let mut fini = None;
    let mut init_array = None;
    let mut init_array_size = 0u64;
    let mut fini_array = None;
    let mut fini_array_size = 0u64;
    let mut dynamic_symtab = None;
    let mut dynamic_strtab = None;
    let mut dynamic_syment = None;
    let mut dynamic_relaent = None;
    let mut dynamic_pltrel = None;
    let mut unsupported_dynamic_relocation = None;
    if let Some((dynamic_vaddr, dynamic_size)) = dynamic {
        if dynamic_size % 16 != 0 || !range_in_load(dynamic_vaddr, dynamic_size) {
            return Err("MC PT_DYNAMIC lies outside a loadable segment".into());
        }
        let mut d = loaded_address(dynamic_vaddr, "PT_DYNAMIC")?;
        let dynamic_end = d
            .checked_add(usize::try_from(dynamic_size).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        while d < dynamic_end {
            let tag = i64::from_le_bytes(unsafe { std::ptr::read(d as *const [u8; 8]) });
            let val = u64::from_le_bytes(unsafe { std::ptr::read((d + 8) as *const [u8; 8]) });
            d += 16;
            match tag {
                0 => break,
                7 => rela = Some(val),            // DT_RELA
                8 => relasz = val,                // DT_RELASZ
                9 => dynamic_relaent = Some(val), // DT_RELAENT
                5 => dynamic_strtab = Some(val),  // DT_STRTAB
                6 => dynamic_symtab = Some(val),  // DT_SYMTAB
                11 => dynamic_syment = Some(val), // DT_SYMENT
                20 => dynamic_pltrel = Some(val), // DT_PLTREL
                23 => jmprel = Some(val),         // DT_JMPREL
                2 => pltrelsz = val,              // DT_PLTRELSZ
                12 => init = Some(val),           // DT_INIT
                13 => fini = Some(val),           // DT_FINI
                25 => init_array = Some(val),     // DT_INIT_ARRAY
                26 => fini_array = Some(val),     // DT_FINI_ARRAY
                27 => init_array_size = val,      // DT_INIT_ARRAYSZ
                28 => fini_array_size = val,      // DT_FINI_ARRAYSZ
                17 | 18 | 19 | 35 | 36 | 37 => unsupported_dynamic_relocation = Some(tag),
                _ => {}
            }
        }
    }

    // .symtab/.strtab/.eh_frame 收集
    let mut symtab: Option<Shdr> = None;
    let mut strtab: Option<Shdr> = None;
    let mut dynsym: Option<Shdr> = None;
    let mut eh_frame: Option<Shdr> = None;
    for i in 0..shnum {
        let s = shdr(i).ok_or_else(bad)?;
        match (s.ty, sec_name(&s)) {
            (2, _) => symtab = Some(s),         // SHT_SYMTAB
            (11, _) => dynsym = Some(s),        // SHT_DYNSYM
            (3, ".strtab") => strtab = Some(s), // SHT_STRTAB
            (_, ".eh_frame") => eh_frame = Some(s),
            _ => {}
        }
    }
    let (sym_s, str_s) = (
        symtab.ok_or("MC 镜像缺 .symtab")?,
        strtab.ok_or("MC 镜像缺 .strtab")?,
    );
    let dyn_s = dynsym.ok_or("MC image lacks .dynsym")?;
    let dyn_str_s = shdr(dyn_s.link as usize).ok_or_else(bad)?;
    if dyn_str_s.ty != 3 {
        return Err("MC .dynsym does not link to a string table".into());
    }
    if dynamic_syment != Some(24) || dyn_s.entsize != 24 {
        return Err("MC DT_SYMENT/.dynsym entry size is not 24".into());
    }
    if dynamic_symtab != Some(dyn_s.addr) || dynamic_strtab != Some(dyn_str_s.addr) {
        return Err("MC dynamic symbol/string table tags disagree with section metadata".into());
    }
    if (rela.is_some() || jmprel.is_some()) && dynamic_relaent != Some(24) {
        return Err("MC DT_RELAENT is not 24".into());
    }
    if jmprel.is_some() && dynamic_pltrel != Some(7) {
        return Err("MC DT_PLTREL is not DT_RELA".into());
    }
    if let Some(tag) = unsupported_dynamic_relocation {
        return Err(format!(
            "MC image contains unsupported dynamic relocation tag {tag}"
        ));
    }
    if rela.is_some() != (relasz != 0) {
        return Err("MC DT_RELA and DT_RELASZ are incomplete".into());
    }
    if jmprel.is_some() != (pltrelsz != 0) {
        return Err("MC DT_JMPREL and DT_PLTRELSZ are incomplete".into());
    }
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
    let dyn_str_at = |off: u32| -> Result<String, String> {
        let start = (dyn_str_s.off + u64::from(off)) as usize;
        let limit = (dyn_str_s.off + dyn_str_s.size) as usize;
        let end = bytes
            .get(start..limit.min(bytes.len()))
            .ok_or("MC image dynstr is out of bounds")?
            .iter()
            .position(|&c| c == 0)
            .map(|p| start + p)
            .ok_or("MC image dynstr is out of bounds")?;
        Ok(std::str::from_utf8(&bytes[start..end])
            .map_err(|_| "MC image dynamic symbol name is not UTF-8")?
            .to_string())
    };
    let dyn_syment = usize::try_from(dyn_s.entsize).map_err(|_| bad())?;
    let dyn_symcount = usize::try_from(dyn_s.size / dyn_s.entsize).map_err(|_| bad())?;
    let dyn_sym_at = |j: usize| -> Option<(u32, u8, u16, u64)> {
        if j >= dyn_symcount {
            return None;
        }
        let b = (dyn_s.off as usize).checked_add(j.checked_mul(dyn_syment)?)?;
        Some((
            u32_at(bytes, b)?,
            bytes.get(b + 4).copied()?,
            u16_at(bytes, b + 6)?,
            u64_at(bytes, b + 8)?,
        ))
    };

    // 符号表（注册面：GLOBAL/WEAK 且已定义；P1 与 runtime bridge 槽即使被
    // ld localize 也必须保留，因为它们是每个 Engine 启动相的内部写入协议）。
    let mut symbols: HashMap<Box<str>, u64> = HashMap::new();
    for j in 0..symcount {
        let (name_off, info, shndx, value) = sym_at(j).ok_or_else(bad)?;
        let bind = info >> 4;
        if name_off == 0 || shndx == 0 || shndx >= 0xff00 {
            continue;
        }
        let name = str_at(name_off)?;
        if bind != 1
            && bind != 2
            && !name.starts_with("__mirvm_p1_target_")
            && !name.starts_with("__mirvm_pthread_")
            && !name.starts_with("__mirvm_signal_")
            && name != "__mirvm_sigaction_target"
            && name != "__mirvm_raise_target"
        {
            continue;
        }
        // 存 vaddr（register/resolve 统一 bias+value，与 archive_fallbacks 同形）
        symbols.insert(name.into_boxed_str(), value);
    }

    // 重定位应用
    let apply = |off: u64, info: u64, addend: i64| -> Result<(), String> {
        let ty = info as u32;
        let sym_idx = (info >> 32) as usize;
        let write_size = if ty == 2 { 4 } else { 8 };
        if ty != 0 && !range_in_load(off, write_size) {
            return Err(format!(
                "MC relocation target {off:#x} is outside a loadable segment"
            ));
        }
        let place = loaded_address(off, "relocation target")?;
        // 符号地址：内部（自 symtab 已定义）→ 外部（RTLD_DEFAULT）→ 弱缺席 0
        let sym_addr = |idx: usize| -> Result<u64, String> {
            if idx == 0 {
                return Ok(0);
            }
            let (name_off, info2, shndx, value) = dyn_sym_at(idx).ok_or_else(bad)?;
            if info2 & 0x0f == 10 {
                return Err("MC dynamic relocation references STT_GNU_IFUNC".into());
            }
            if shndx != 0 && shndx < 0xff00 {
                return Ok(loaded_address(value, "defined symbol")? as u64);
            }
            if shndx == 0xfff1 {
                return Ok(value); // SHN_ABS
            }
            if shndx >= 0xff00 && shndx != 0 {
                return Err(format!(
                    "MC dynamic symbol has unsupported reserved section index {shndx:#x}"
                ));
            }
            let name = dyn_str_at(name_off)?;
            let c = std::ffi::CString::new(name.as_str()).map_err(|_| "符号名含 NUL")?;
            let p = crate::os::dll::sym(0, &c);
            if p != 0 {
                return Ok(p as u64);
            }
            if (info2 >> 4) == 2 {
                return Ok(0); // WEAK 缺席 = 0
            }
            Err(format!(
                "MC 重定位符号 `{name}` 未命中（RTLD_DEFAULT 均无）"
            ))
        };
        match ty {
            0 => Ok(()), // NONE
            8 => {
                // RELATIVE：*(place) = base + addend
                unsafe {
                    std::ptr::write_unaligned(
                        place as *mut u64,
                        loaded_signed_address(addend, "R_X86_64_RELATIVE addend")?,
                    )
                };
                Ok(())
            }
            1 => {
                // 64：*(place) = sym + addend
                let s = sym_addr(sym_idx)?;
                unsafe {
                    std::ptr::write_unaligned(place as *mut u64, s.wrapping_add(addend as u64))
                };
                Ok(())
            }
            2 => {
                // PC32：*(place) = sym + addend - place
                let s = sym_addr(sym_idx)?;
                let value = i128::from(s) + i128::from(addend) - place as i128;
                let value = i32::try_from(value)
                    .map_err(|_| "MC R_X86_64_PC32 relocation is outside signed 32-bit range")?;
                unsafe { std::ptr::write_unaligned(place as *mut i32, value) };
                Ok(())
            }
            6 | 7 => {
                // GLOB_DAT / JUMP_SLOT：*(place) = sym
                let s = sym_addr(sym_idx)?;
                unsafe { std::ptr::write_unaligned(place as *mut u64, s) };
                Ok(())
            }
            16..=18 => Err("MC 镜像含 TLS 重定位（DTPMOD/DTPOFF 未接）".into()),
            5 => Err("MC 镜像含 COPY 重定位（不接）".into()),
            other => Err(format!("MC 镜像含未支持重定位类型 {other}")),
        }
    };
    if let Some(r0) = rela {
        if !relasz.is_multiple_of(24) || !range_in_load(r0, relasz) {
            return Err("MC DT_RELA table lies outside a loadable segment".into());
        }
        let cnt = (relasz / 24) as usize;
        let start = loaded_address(r0, "DT_RELA")?;
        for i in 0..cnt {
            let b = start + i * 24;
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
        if !pltrelsz.is_multiple_of(24) || !range_in_load(j0, pltrelsz) {
            return Err("MC DT_JMPREL table lies outside a loadable segment".into());
        }
        let cnt = (pltrelsz / 24) as usize;
        let start = loaded_address(j0, "DT_JMPREL")?;
        for i in 0..cnt {
            let b = start + i * 24;
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

    let read_array = |address: Option<u64>, size: u64, what: &str| -> Result<Vec<usize>, String> {
        let Some(address) = address else {
            if size == 0 {
                return Ok(Vec::new());
            }
            return Err(format!("MC {what} has size but no address"));
        };
        if !size.is_multiple_of(8) || size > 1 << 20 {
            return Err(format!("MC {what} has invalid size {size}"));
        }
        address
            .checked_add(size)
            .ok_or_else(|| format!("MC {what} range overflow"))?;
        if !range_in_load(address, size) {
            return Err(format!("MC {what} lies outside a loadable segment"));
        }
        let start = loaded_address(address, what)?;
        let mut functions = Vec::with_capacity((size / 8) as usize);
        for index in 0..(size / 8) as usize {
            let value = unsafe { (start as *const usize).add(index).read_unaligned() };
            if value != 0 && value != usize::MAX {
                functions.push(value);
            }
        }
        Ok(functions)
    };
    let mut initializers = Vec::new();
    if let Some(address) = init {
        initializers.push(loaded_address(address, "DT_INIT")?);
    }
    initializers.extend(read_array(init_array, init_array_size, "DT_INIT_ARRAY")?);
    let mut finalizers = read_array(fini_array, fini_array_size, "DT_FINI_ARRAY")?;
    finalizers.reverse();
    if let Some(address) = fini {
        finalizers.push(loaded_address(address, "DT_FINI")?);
    }

    // Parse unwind records now, but do not publish them to libgcc until every
    // fallible segment-protection operation has succeeded.
    let mut registered_frames = Vec::new();
    if let Some(eh) = eh_frame {
        if !range_in_load(eh.addr, eh.size) {
            return Err("MC .eh_frame lies outside a loadable segment".into());
        }
        let start = loaded_address(eh.addr, ".eh_frame")?;
        let end = start
            .checked_add(usize::try_from(eh.size).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        let mut cur = start;
        while cur + 8 <= end {
            let len = u32::from_le_bytes(unsafe { std::ptr::read(cur as *const [u8; 4]) }) as usize;
            if len == 0 {
                break;
            }
            let record_end = cur
                .checked_add(len + 4)
                .ok_or_else(|| "MC .eh_frame record overflow".to_string())?;
            if record_end > end {
                return Err("MC .eh_frame record exceeds section".into());
            }
            let cie_ptr =
                u32::from_le_bytes(unsafe { std::ptr::read((cur + 4) as *const [u8; 4]) });
            if cie_ptr != 0 {
                registered_frames.push(cur);
            }
            cur = record_end;
        }
    }

    for (_, vaddr, _, memsz, flags) in &loads {
        if flags & 3 == 3 {
            return Err("MC image contains a writable executable PT_LOAD segment".into());
        }
        let start = loaded_address(*vaddr as u64, "PT_LOAD protection")? & !(page - 1);
        let segment_end = loaded_address(*vaddr as u64, "PT_LOAD protection")?
            .checked_add(*memsz)
            .ok_or("MC PT_LOAD protection range overflow")?;
        let end = segment_end
            .checked_add(page - 1)
            .ok_or("MC PT_LOAD protection alignment overflow")?
            & !(page - 1);
        crate::os::mem::protect(start as *mut u8, end - start, seg_prot(*flags))
            .map_err(|e| format!("MC image segment protection failed: {e}"))?;
    }
    let executable_ranges = loads
        .iter()
        .filter(|(_, _, _, _, flags)| flags & 1 != 0)
        .map(|(_, vaddr, _, memsz, _)| {
            let start = loaded_address(*vaddr as u64, "executable PT_LOAD")?;
            let end = start
                .checked_add(*memsz)
                .ok_or("MC executable PT_LOAD range overflow")?;
            Ok((start, end))
        })
        .collect::<Result<Vec<_>, String>>()?
        .into_boxed_slice();

    unsafe extern "C" {
        fn __register_frame(fde: *const u8);
    }
    for &frame in &registered_frames {
        unsafe { __register_frame(frame as *const u8) };
    }

    mapping_guard.armed = false;
    Ok(McImage {
        mapping,
        load_bias,
        size,
        symbols,
        executable_ranges,
        lifecycle: super::native_instance::NativeLifecycle::new(initializers, finalizers),
        registered_frames: registered_frames.into_boxed_slice(),
        committed: AtomicBool::new(false),
    })
}

impl McImage {
    pub(crate) fn load_bias(&self) -> usize {
        self.load_bias
    }

    pub(crate) fn executable_ranges(&self) -> &[(usize, usize)] {
        &self.executable_ranges
    }

    pub(crate) fn commit(&self) {
        self.committed.store(true, Ordering::Release);
    }

    pub(crate) fn run_initializers(&self, args: &mut super::native_instance::InitializerArgs) {
        self.lifecycle.run_initializers(args);
    }

    pub(crate) fn run_finalizers(&self) {
        self.lifecycle.run_finalizers();
    }
}

impl Drop for McImage {
    fn drop(&mut self) {
        if self.committed.load(Ordering::Acquire) {
            return;
        }
        unsafe extern "C" {
            fn __deregister_frame(fde: *const u8);
        }
        for &frame in self.registered_frames.iter().rev() {
            unsafe { __deregister_frame(frame as *const u8) };
        }
        unsafe { crate::os::mem::unmap(self.mapping as *mut u8, self.size) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    static SIGNAL_NUMBER: AtomicU64 = AtomicU64::new(0);
    static SIGNAL_HANDLER: AtomicU64 = AtomicU64::new(0);
    static SIGNAL_OWNER: AtomicU64 = AtomicU64::new(0);

    extern "C" fn capture_signal(signum: i32, handler: usize, owner: u64) -> usize {
        SIGNAL_NUMBER.store(signum as u64, Ordering::Relaxed);
        SIGNAL_HANDLER.store(handler as u64, Ordering::Relaxed);
        SIGNAL_OWNER.store(owner, Ordering::Relaxed);
        handler
    }

    struct FixtureDir(std::path::PathBuf);

    impl FixtureDir {
        fn new() -> Self {
            let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mirvm-mcload-relocation-{}-{serial}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn relocation_fixture() -> (FixtureDir, Vec<u8>) {
        let directory = FixtureDir::new();
        let source = directory.0.join("probe.S");
        let library = directory.0.join("probe.so");
        std::fs::write(
            &source,
            r#"
.intel_syntax noprefix
.text
.globl absolute_probe
.type absolute_probe,@function
absolute_probe:
    mov rax, QWORD PTR [rip + absolute_pointer]
    ret
.size absolute_probe,.-absolute_probe
.p2align 3
absolute_pointer:
    .quad absolute_target

.globl dynsym_probe
.type dynsym_probe,@function
dynsym_probe:
    mov rax, QWORD PTR [rip + target_value@GOTPCREL]
    mov rax, QWORD PTR [rax]
    ret
.size dynsym_probe,.-dynsym_probe

.globl absolute_target
.set absolute_target, 0x1234
.data
.globl target_value
.type target_value,@object
.size target_value,8
target_value:
    .quad 0x1122334455667788
.section .note.GNU-stack,"",@progbits
"#,
        )
        .unwrap();
        let output = std::process::Command::new("cc")
            .args([
                "-shared",
                "-nostdlib",
                "-fPIC",
                "-Wl,-z,defs",
                "-Wl,-z,notext",
            ])
            .arg(&source)
            .arg("-o")
            .arg(&library)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to build MC relocation fixture:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(library).unwrap();
        (directory, bytes)
    }

    #[test]
    fn symbol_lookup_is_scoped_to_the_supplied_images() {
        let image = |base, value| McImage {
            mapping: base,
            load_bias: base,
            size: 0x1000,
            symbols: HashMap::from([(Box::<str>::from("same_symbol"), value)]),
            executable_ranges: Box::new([]),
            lifecycle: Default::default(),
            registered_frames: Box::new([]),
            committed: AtomicBool::new(true),
        };
        let first = image(0x1000, 0x20);
        let second = image(0x4000, 0x80);

        assert_eq!(
            resolve(std::slice::from_ref(&first), "same_symbol"),
            Some(0x1020)
        );
        assert_eq!(
            resolve(std::slice::from_ref(&second), "same_symbol"),
            Some(0x4080)
        );
        assert_eq!(resolve(&[], "same_symbol"), None);
    }

    #[test]
    fn global_asm_signal_call_uses_the_owned_runtime_bridge() {
        const OWNER: u64 = 0x1020_3040_5060_7080;
        const HANDLER: usize = 0x1234_5678;
        let path = crate::lower::global_asm::assemble(
            r#"
.intel_syntax noprefix
.text
.globl mirvm_mc_signal_bridge_probe
.type mirvm_mc_signal_bridge_probe,@function
mirvm_mc_signal_bridge_probe:
    mov edi, 10
    mov esi, 0x12345678
    jmp signal@PLT
.size mirvm_mc_signal_bridge_probe,.-mirvm_mc_signal_bridge_probe
.section .note.GNU-stack,"",@progbits
"#,
        )
        .unwrap();
        let bytes = std::fs::read(&*path).unwrap();
        let image = load(&bytes).unwrap();
        let patch = |name: &str, value: u64| {
            let offset = image
                .symbols
                .get(name)
                .unwrap_or_else(|| panic!("MC runtime bridge has no `{name}` slot"));
            let address = image
                .load_bias()
                .checked_add(*offset as usize)
                .expect("MC runtime bridge slot address");
            unsafe { (address as *mut u64).write(value) };
        };
        patch("__mirvm_signal_owner", OWNER);
        patch(
            "__mirvm_signal_target",
            capture_signal as *const () as usize as u64,
        );

        let probe: unsafe extern "C" fn() -> usize = unsafe {
            std::mem::transmute(
                resolve(std::slice::from_ref(&image), "mirvm_mc_signal_bridge_probe").unwrap(),
            )
        };
        assert_eq!(unsafe { probe() }, HANDLER);
        assert_eq!(SIGNAL_NUMBER.load(Ordering::Relaxed), 10);
        assert_eq!(SIGNAL_HANDLER.load(Ordering::Relaxed), HANDLER as u64);
        assert_eq!(SIGNAL_OWNER.load(Ordering::Relaxed), OWNER);
    }

    #[test]
    fn dynamic_relocations_use_dynsym_and_run_before_segment_protection() {
        let (_directory, bytes) = relocation_fixture();
        let image = load(&bytes).unwrap();
        let absolute: unsafe extern "C" fn() -> u64 = unsafe {
            std::mem::transmute(
                resolve(std::slice::from_ref(&image), "absolute_probe")
                    .expect("absolute probe symbol"),
            )
        };
        let dynsym: unsafe extern "C" fn() -> u64 = unsafe {
            std::mem::transmute(
                resolve(std::slice::from_ref(&image), "dynsym_probe").expect("dynsym probe symbol"),
            )
        };

        // absolute_pointer is an R_X86_64_64 text relocation against a
        // SHN_ABS dynamic symbol. Applying it after RX protection would fault.
        assert_eq!(unsafe { absolute() }, 0x1234);
        // target_value's GOT relocation indexes .dynsym; the same index in
        // .symtab intentionally denotes a different entry in this fixture.
        assert_eq!(unsafe { dynsym() }, 0x1122_3344_5566_7788);
    }
}
