//! The ELF half of the symbol tables: `.symtab` and `.dynsym`.
//!
//! Both are arrays of the same entries against a string table, and the difference that matters to a
//! caller is which of the two a name appears in: a definition that never entered `.dynsym` is one
//! `dlsym` cannot reach, which is the fallback table's whole subject. The dispatcher that chooses
//! between this half and the Mach-O one is [`super`].

use std::collections::HashMap;

use super::{Error, Export};
use crate::native::object::elf::{
    self, SHN_RESERVED, SHN_UNDEF, SHT_DYNSYM, SHT_SYMTAB, STB_GLOBAL, STB_WEAK,
};
use crate::utils::bytes::{read_u16, read_u32, read_u64};

/// One `.symtab`/`.dynsym` entry, resolved against its string table.
///
/// `name_offset` is the raw `st_name` and travels with the entry because offset 0 is the string
/// table's empty name: an entry pointing there names nothing and is not a symbol, which is a
/// different question from whether the resolved string is empty.
struct Symbol<'a> {
    name: &'a str,
    name_offset: u32,
    value: u64,
    section_index: u16,
    binding: u8,
}

/// Read entry `index` of `table` (a symbol table section) against `strtab`.
///
/// `str_end` is the string table's end offset, computed once by the caller. `None` means the entry
/// or the name it points at lies outside the image.
fn symbol_at<'a>(
    bytes: &'a [u8],
    table: &elf::Section,
    strtab: &elf::Section,
    str_end: usize,
    index: usize,
) -> Option<Symbol<'a>> {
    let base = usize::try_from(table.offset)
        .ok()?
        .checked_add(index.checked_mul(usize::try_from(table.entsize).ok()?)?)?;
    let name_offset = read_u32(bytes, base + elf::sym::NAME)?;
    let binding = bytes.get(base + elf::sym::INFO).copied()? >> 4;
    let section_index = read_u16(bytes, base + elf::sym::SHNDX)?;
    let value = read_u64(bytes, base + elf::sym::VALUE)?;
    let name_start = usize::try_from(strtab.offset)
        .ok()?
        .checked_add(usize::try_from(name_offset).ok()?)?;
    let name_end = name_start
        + bytes
            .get(name_start..str_end.min(bytes.len()))?
            .iter()
            .position(|&b| b == 0)?;
    let name = std::str::from_utf8(bytes.get(name_start..name_end)?).ok()?;
    Some(Symbol {
        name,
        name_offset,
        value,
        section_index,
        binding,
    })
}

/// Resolve the given symbol table section (SHT_SYMTAB / SHT_DYNSYM; entries have the same
/// format): defined symbol name -> st_value (file virtual address, relative to the load base).
pub(super) fn symbol_table_values(
    so_path: &str,
    want_sht: u32,
) -> Result<HashMap<Box<str>, u64>, Error> {
    let bytes = std::fs::read(so_path)
        .map_err(|e| Error::io(format!("cannot read the shared library `{so_path}`"), e))?;
    let bad = || {
        Error::malformed(format!(
            "archive shared library `{so_path}` is not the expected ELF64 LE (or is corrupted)"
        ))
    };
    let header = elf::FileHeader::parse(&bytes).ok_or_else(bad)?;
    let sections = elf::sections(&bytes, &header).ok_or_else(bad)?;
    for section in &sections {
        if section.ty != want_sht {
            continue;
        }
        if section.entsize < elf::SYM_ENTRY_SIZE as u64 {
            return Err(bad());
        }
        let strtab = sections
            .get(usize::try_from(section.link).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        let str_end = usize::try_from(strtab.offset + strtab.size).map_err(|_| bad())?;
        let count = usize::try_from(section.size / section.entsize.max(1)).map_err(|_| bad())?;
        let mut out = HashMap::new();
        for index in 0..count {
            let symbol = symbol_at(&bytes, section, strtab, str_end, index).ok_or_else(bad)?;
            // Skip SHN_UNDEF (0) and reserved section indices (0xff00+)
            if symbol.name_offset == 0
                || symbol.section_index == SHN_UNDEF
                || symbol.section_index >= SHN_RESERVED
            {
                continue;
            }
            out.insert(Box::from(symbol.name), symbol.value);
        }
        return Ok(out);
    }
    // No requested symbol table section (should not happen for a materialized artifact,
    // stripped or not, but harmless): treat as empty
    Ok(HashMap::new())
}

/// SHN_UNDEF enumeration over a single ELF64 LE byte slice (GLOBAL/WEAK bindings; the same
/// structural walk as symbol_table_values, but selecting shndx == 0 with no filtering).
pub(super) fn elf_undefined_symbols(bytes: &[u8]) -> Result<Vec<Box<str>>, Error> {
    let bad = || Error::malformed("not the expected ELF64 LE (or corrupted)");
    let header = elf::FileHeader::parse(bytes).ok_or_else(bad)?;
    let sections = elf::sections(bytes, &header).ok_or_else(bad)?;
    for section in &sections {
        if section.ty != SHT_SYMTAB {
            continue;
        }
        if section.entsize < elf::SYM_ENTRY_SIZE as u64 {
            return Err(bad());
        }
        let strtab = sections
            .get(usize::try_from(section.link).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        let str_end = usize::try_from(strtab.offset + strtab.size).map_err(|_| bad())?;
        let count = usize::try_from(section.size / section.entsize.max(1)).map_err(|_| bad())?;
        let mut out = Vec::new();
        for index in 0..count {
            let symbol = symbol_at(bytes, section, strtab, str_end, index).ok_or_else(bad)?;
            // Take only undefined (SHN_UNDEF) global/weak bindings (LOCAL is the member's
            // internal business)
            if symbol.name_offset == 0
                || symbol.section_index != SHN_UNDEF
                || (symbol.binding != STB_GLOBAL && symbol.binding != STB_WEAK)
            {
                continue;
            }
            out.push(Box::from(symbol.name));
        }
        return Ok(out);
    }
    Ok(Vec::new())
}

/// The dynamic table's defined entries: `.dynsym`, which is the surface other images bind to.
pub(super) fn elf_exports(bytes: &[u8], path: &str) -> Result<Vec<Export>, Error> {
    let bad = || {
        Error::malformed(format!(
            "archive shared library `{path}` is not the expected ELF64 LE (or is corrupted)"
        ))
    };
    let header = elf::FileHeader::parse(bytes).ok_or_else(bad)?;
    let sections = elf::sections(bytes, &header).ok_or_else(bad)?;
    for section in &sections {
        if section.ty != SHT_DYNSYM {
            continue;
        }
        if section.entsize < elf::SYM_ENTRY_SIZE as u64 {
            return Err(bad());
        }
        let strtab = sections
            .get(usize::try_from(section.link).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        let str_end = usize::try_from(strtab.offset + strtab.size).map_err(|_| bad())?;
        let count = usize::try_from(section.size / section.entsize.max(1)).map_err(|_| bad())?;
        let mut out = Vec::new();
        for index in 0..count {
            let symbol = symbol_at(bytes, section, strtab, str_end, index).ok_or_else(bad)?;
            if symbol.name_offset == 0
                || symbol.section_index == SHN_UNDEF
                || symbol.section_index >= SHN_RESERVED
            {
                continue;
            }
            out.push(Export {
                name: Box::from(symbol.name),
                // A unique is merged by the loader rather than chosen between, so it is not a
                // definition two images can conflict over.
                weak: symbol.binding == STB_WEAK || symbol.binding == elf::STB_GNU_UNIQUE,
            });
        }
        return Ok(out);
    }
    Ok(Vec::new())
}
