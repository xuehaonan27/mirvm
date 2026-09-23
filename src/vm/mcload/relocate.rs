//! Self-relocation.
//!
//! The dynamic table names the relocation tables and they live in the mapped image; every record
//! is applied to those bytes before the segments get their final protection, because a `RELATIVE`
//! or `ABS64` write into a read-only segment would fault once that protection is on.
//!
//! A relocation's `r_info` indexes `.dynsym`, not `.symtab`: the image's own defined symbols come
//! first, then the host through `RTLD_DEFAULT`, then zero for a weak symbol the host does not
//! define.

use crate::arch::reloc;
use crate::native::elf;

use super::bad;
use super::dynamic::Dynamic;
use super::map::Mapping;
use super::symbols::Symbols;

/// Refuse the tag shapes that would make a relocation table unreadable, then apply `DT_RELA` and
/// `DT_JMPREL` in that order.
pub(super) fn apply(
    mapping: &Mapping,
    dynamic: &Dynamic,
    symbols: &Symbols<'_>,
) -> Result<(), String> {
    if (dynamic.rela.is_some() || dynamic.jmprel.is_some())
        && dynamic.relaent != Some(elf::RELA_ENTRY_SIZE as u64)
    {
        return Err("MC DT_RELAENT is not 24".into());
    }
    if dynamic.jmprel.is_some() && dynamic.pltrel != Some(elf::DT_RELA as u64) {
        return Err("MC DT_PLTREL is not DT_RELA".into());
    }
    if let Some(tag) = dynamic.unsupported_relocation {
        return Err(format!(
            "MC image contains unsupported dynamic relocation tag {tag}"
        ));
    }
    if dynamic.rela.is_some() != (dynamic.relasz != 0) {
        return Err("MC DT_RELA and DT_RELASZ are incomplete".into());
    }
    if dynamic.jmprel.is_some() != (dynamic.pltrelsz != 0) {
        return Err("MC DT_JMPREL and DT_PLTRELSZ are incomplete".into());
    }
    if let Some(base) = dynamic.rela {
        apply_table(mapping, symbols, base, dynamic.relasz, "DT_RELA")?;
    }
    if let Some(base) = dynamic.jmprel {
        apply_table(mapping, symbols, base, dynamic.pltrelsz, "DT_JMPREL")?;
    }
    Ok(())
}

/// Apply one `RELA` table, whose records are an offset, an `r_info` and an addend.
fn apply_table(
    mapping: &Mapping,
    symbols: &Symbols<'_>,
    base: u64,
    size: u64,
    what: &str,
) -> Result<(), String> {
    let entry_size = elf::RELA_ENTRY_SIZE as u64;
    if !size.is_multiple_of(entry_size) || !mapping.contains(base, size) {
        return Err(format!("MC {what} table lies outside a loadable segment"));
    }
    let count = (size / entry_size) as usize;
    let start = mapping.address(base, what)?;
    for index in 0..count {
        let entry = start + index * elf::RELA_ENTRY_SIZE;
        let (offset, info, addend) = unsafe {
            (
                std::ptr::read_unaligned((entry + elf::rela::OFFSET) as *const u64),
                std::ptr::read_unaligned((entry + elf::rela::INFO) as *const u64),
                std::ptr::read_unaligned((entry + elf::rela::ADDEND) as *const i64),
            )
        };
        apply_one(mapping, symbols, offset, info, addend)?;
    }
    Ok(())
}

/// Apply one relocation record. The kind decides the write's width, and the write only happens
/// after the target is known to lie inside a loadable segment.
fn apply_one(
    mapping: &Mapping,
    symbols: &Symbols<'_>,
    offset: u64,
    info: u64,
    addend: i64,
) -> Result<(), String> {
    let ty = info as u32;
    let symbol_index = (info >> 32) as usize;
    let kind = reloc::classify(ty);
    let write_size = reloc::field_width(kind) as u64;
    if kind != reloc::Kind::None && !mapping.contains(offset, write_size) {
        return Err(format!(
            "MC relocation target {offset:#x} is outside a loadable segment"
        ));
    }
    let place = mapping.address(offset, "relocation target")?;
    let symbol = |index: usize| -> Result<u64, String> {
        if index == 0 {
            return Ok(0);
        }
        let entry = symbols.dynamic_symbol(index).ok_or_else(bad)?;
        if elf::sym_type(entry.info) == elf::STT_GNU_IFUNC {
            return Err("MC dynamic relocation references STT_GNU_IFUNC".into());
        }
        if entry.shndx != elf::SHN_UNDEF && entry.shndx < elf::SHN_RESERVED {
            return Ok(mapping.address(entry.value, "defined symbol")? as u64);
        }
        if entry.shndx == elf::SHN_ABS {
            return Ok(entry.value);
        }
        if entry.shndx >= elf::SHN_RESERVED && entry.shndx != elf::SHN_UNDEF {
            return Err(format!(
                "MC dynamic symbol has unsupported reserved section index {:#x}",
                entry.shndx
            ));
        }
        let name = symbols.dynamic_name(entry.name)?;
        let c = std::ffi::CString::new(name.as_str()).map_err(|_| "symbol name contains NUL")?;
        let p = crate::os::dll::sym(0, &c);
        if p != 0 {
            return Ok(p as u64);
        }
        if elf::sym_bind(entry.info) == elf::STB_WEAK {
            return Ok(0); // WEAK missing = 0
        }
        Err(format!(
            "MC relocation symbol `{name}` not found (neither archive fallback nor RTLD_DEFAULT)"
        ))
    };
    match kind {
        reloc::Kind::None => Ok(()),
        reloc::Kind::Relative => {
            unsafe {
                std::ptr::write_unaligned(
                    place as *mut u64,
                    mapping.signed_address(addend, "R_X86_64_RELATIVE addend")?,
                )
            };
            Ok(())
        }
        reloc::Kind::Abs64 => {
            let address = symbol(symbol_index)?;
            unsafe {
                std::ptr::write_unaligned(place as *mut u64, address.wrapping_add(addend as u64))
            };
            Ok(())
        }
        reloc::Kind::Pc32 => {
            let address = symbol(symbol_index)?;
            let value = i128::from(address) + i128::from(addend) - place as i128;
            let value = i32::try_from(value)
                .map_err(|_| "MC R_X86_64_PC32 relocation is outside signed 32-bit range")?;
            unsafe { std::ptr::write_unaligned(place as *mut i32, value) };
            Ok(())
        }
        reloc::Kind::Symbol => {
            let address = symbol(symbol_index)?;
            unsafe { std::ptr::write_unaligned(place as *mut u64, address) };
            Ok(())
        }
        reloc::Kind::Tls => {
            Err("MC image contains TLS relocation (DTPMOD/DTPOFF not handled)".into())
        }
        reloc::Kind::Copy => Err("MC image contains COPY relocation (not handled)".into()),
        reloc::Kind::Unsupported => Err(format!(
            "MC image contains unsupported relocation type {ty}"
        )),
    }
}
