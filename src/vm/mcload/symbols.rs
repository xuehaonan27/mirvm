//! The symbol tables an image carries.
//!
//! `.symtab` with `.strtab` is the registration side: every defined GLOBAL/WEAK symbol is
//! published so guest code can name it. `.dynsym` with the string table its `sh_link` points at
//! is the relocation side: a relocation indexes it through `r_info`. The dynamic tags that
//! describe these tables must agree with the section headers, or the image is not the
//! self-produced family this loader reads.

use std::collections::HashMap;

use crate::native::elf;

use super::bad;
use super::dynamic::Dynamic;
use super::parse::{Image, Tables};

/// One symbol table entry, before the name it points at is resolved.
#[derive(Clone, Copy)]
pub(super) struct RawSymbol {
    pub name: u32,
    pub info: u8,
    pub shndx: u16,
    pub value: u64,
}

/// The four symbol-table sections and the dynamic-symbol count that bounds indexing into
/// `.dynsym`.
pub(super) struct Symbols<'a> {
    bytes: &'a [u8],
    symtab: elf::Section,
    strtab: elf::Section,
    dynsym: elf::Section,
    dynstr: elf::Section,
    dynsym_entry: usize,
    dynsym_count: usize,
}

impl<'a> Symbols<'a> {
    /// Locate the tables and check the dynamic tags that describe them against the section
    /// headers; a disagreement means the bytes are not the image the dynamic table claims.
    pub(super) fn read(
        image: &Image<'a>,
        tables: &Tables,
        dynamic: &Dynamic,
    ) -> Result<Self, String> {
        let symtab = tables.symtab.ok_or("MC image lacks .symtab")?;
        let strtab = tables.strtab.ok_or("MC image lacks .strtab")?;
        let dynsym = tables.dynsym.ok_or("MC image lacks .dynsym")?;
        let dynstr = image.section_at(dynsym.link as usize).ok_or_else(bad)?;
        if dynstr.ty != elf::SHT_STRTAB {
            return Err("MC .dynsym does not link to a string table".into());
        }
        if dynamic.syment != Some(elf::SYM_ENTRY_SIZE as u64)
            || dynsym.entsize != elf::SYM_ENTRY_SIZE as u64
        {
            return Err("MC DT_SYMENT/.dynsym entry size is not 24".into());
        }
        if dynamic.symtab != Some(dynsym.addr) || dynamic.strtab != Some(dynstr.addr) {
            return Err(
                "MC dynamic symbol/string table tags disagree with section metadata".into(),
            );
        }
        let dynsym_entry = usize::try_from(dynsym.entsize).map_err(|_| bad())?;
        let dynsym_count = usize::try_from(dynsym.size / dynsym.entsize).map_err(|_| bad())?;
        Ok(Self {
            bytes: image.bytes,
            symtab,
            strtab,
            dynsym,
            dynstr,
            dynsym_entry,
            dynsym_count,
        })
    }

    /// Every defined GLOBAL/WEAK symbol mapped to its ELF virtual address, which register and
    /// resolve both turn into a real one by adding the load bias. The runtime-bridge slots are
    /// kept whatever their binding: the Engine writes them after load as part of the per-Engine
    /// startup protocol, so a linker that localized them must not hide them.
    pub(super) fn registration_map(&self) -> Result<HashMap<Box<str>, u64>, String> {
        let entry_size = self.symtab.entsize.max(elf::SYM_ENTRY_SIZE as u64) as usize;
        let count = (self.symtab.size as usize) / entry_size;
        let mut symbols = HashMap::new();
        for index in 0..count {
            let symbol = self.static_symbol(index, entry_size).ok_or_else(bad)?;
            let bind = elf::sym_bind(symbol.info);
            if symbol.name == 0
                || symbol.shndx == elf::SHN_UNDEF
                || symbol.shndx >= elf::SHN_RESERVED
            {
                continue;
            }
            let name = self.static_name(symbol.name)?;
            if bind != elf::STB_GLOBAL
                && bind != elf::STB_WEAK
                && !name.starts_with("__mirvm_p1_target_")
                && !name.starts_with("__mirvm_pthread_")
                && !name.starts_with("__mirvm_signal_")
                && name != "__mirvm_sigaction_target"
                && name != "__mirvm_raise_target"
            {
                continue;
            }
            symbols.insert(name.into_boxed_str(), symbol.value);
        }
        Ok(symbols)
    }

    /// The dynamic symbol at `index`, or `None` past the end of `.dynsym`.
    pub(super) fn dynamic_symbol(&self, index: usize) -> Option<RawSymbol> {
        if index >= self.dynsym_count {
            return None;
        }
        let base =
            (self.dynsym.offset as usize).checked_add(index.checked_mul(self.dynsym_entry)?)?;
        Some(RawSymbol {
            name: elf::u32_at(self.bytes, base + elf::sym::NAME)?,
            info: *self.bytes.get(base + elf::sym::INFO)?,
            shndx: elf::u16_at(self.bytes, base + elf::sym::SHNDX)?,
            value: elf::u64_at(self.bytes, base + elf::sym::VALUE)?,
        })
    }

    /// The NUL-terminated name at `offset` in the string table `.dynsym` links to.
    pub(super) fn dynamic_name(&self, offset: u32) -> Result<String, String> {
        let start = (self.dynstr.offset + u64::from(offset)) as usize;
        let limit = (self.dynstr.offset + self.dynstr.size) as usize;
        let end = self
            .bytes
            .get(start..limit.min(self.bytes.len()))
            .ok_or("MC image dynstr is out of bounds")?
            .iter()
            .position(|&c| c == 0)
            .map(|position| start + position)
            .ok_or("MC image dynstr is out of bounds")?;
        Ok(std::str::from_utf8(&self.bytes[start..end])
            .map_err(|_| "MC image dynamic symbol name is not UTF-8")?
            .to_string())
    }

    fn static_symbol(&self, index: usize, entry_size: usize) -> Option<RawSymbol> {
        let base = (self.symtab.offset as usize).checked_add(index.checked_mul(entry_size)?)?;
        Some(RawSymbol {
            name: elf::u32_at(self.bytes, base + elf::sym::NAME)?,
            info: *self.bytes.get(base + elf::sym::INFO)?,
            shndx: elf::u16_at(self.bytes, base + elf::sym::SHNDX)?,
            value: elf::u64_at(self.bytes, base + elf::sym::VALUE)?,
        })
    }

    /// The NUL-terminated name at `offset` in `.strtab`.
    fn static_name(&self, offset: u32) -> Result<String, String> {
        let start = (self.strtab.offset + u64::from(offset)) as usize;
        let limit = (self.strtab.offset + self.strtab.size) as usize;
        let end = self.bytes[start..limit.min(self.bytes.len())]
            .iter()
            .position(|&c| c == 0)
            .map(|position| start + position)
            .ok_or("MC image strtab out of bounds")?;
        Ok(std::str::from_utf8(&self.bytes[start..end])
            .map_err(|_| "MC image symbol name is not UTF-8")?
            .to_string())
    }
}
