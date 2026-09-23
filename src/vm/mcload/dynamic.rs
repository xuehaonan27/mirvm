//! The image's `PT_DYNAMIC` table: where its relocation and symbol tables are, and which
//! constructors and destructors it asked for.
//!
//! Every value is an ELF virtual address until a caller turns it into a real one, and the tags
//! this loader refuses are named so the refusal reports the tag rather than a number.

use crate::native::elf;

use super::bad;
use super::map::Mapping;

/// The dynamic tags the loader acts on.
#[derive(Default)]
pub(super) struct Dynamic {
    pub rela: Option<u64>,
    pub relasz: u64,
    pub relaent: Option<u64>,
    pub jmprel: Option<u64>,
    pub pltrelsz: u64,
    pub pltrel: Option<u64>,
    pub symtab: Option<u64>,
    pub strtab: Option<u64>,
    pub syment: Option<u64>,
    pub init: Option<u64>,
    pub fini: Option<u64>,
    pub init_array: Option<u64>,
    pub init_array_size: u64,
    pub fini_array: Option<u64>,
    pub fini_array_size: u64,
    /// The first thread-local-storage relocation tag the image carries, which the loader refuses.
    pub unsupported_relocation: Option<i64>,
}

impl Dynamic {
    /// Read the table the `PT_DYNAMIC` program header points at, stopping at `DT_NULL`.
    pub(super) fn read(mapping: &Mapping) -> Result<Self, String> {
        let Some((vaddr, size)) = mapping.dynamic() else {
            return Ok(Self::default());
        };
        if !size.is_multiple_of(elf::DYN_ENTRY_SIZE as u64) || !mapping.contains(vaddr, size) {
            return Err("MC PT_DYNAMIC lies outside a loadable segment".into());
        }
        let mut dynamic = Self::default();
        let mut entry = mapping.address(vaddr, "PT_DYNAMIC")?;
        let end = entry
            .checked_add(usize::try_from(size).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        while entry < end {
            let tag = i64::from_le_bytes(unsafe {
                std::ptr::read((entry + elf::dynamic::TAG) as *const [u8; 8])
            });
            let value = u64::from_le_bytes(unsafe {
                std::ptr::read((entry + elf::dynamic::VALUE) as *const [u8; 8])
            });
            entry += elf::DYN_ENTRY_SIZE;
            match tag {
                elf::DT_NULL => break,
                elf::DT_RELA => dynamic.rela = Some(value),
                elf::DT_RELASZ => dynamic.relasz = value,
                elf::DT_RELAENT => dynamic.relaent = Some(value),
                elf::DT_STRTAB => dynamic.strtab = Some(value),
                elf::DT_SYMTAB => dynamic.symtab = Some(value),
                elf::DT_SYMENT => dynamic.syment = Some(value),
                elf::DT_PLTREL => dynamic.pltrel = Some(value),
                elf::DT_JMPREL => dynamic.jmprel = Some(value),
                elf::DT_PLTRELSZ => dynamic.pltrelsz = value,
                elf::DT_INIT => dynamic.init = Some(value),
                elf::DT_FINI => dynamic.fini = Some(value),
                elf::DT_INIT_ARRAY => dynamic.init_array = Some(value),
                elf::DT_FINI_ARRAY => dynamic.fini_array = Some(value),
                elf::DT_INIT_ARRAYSZ => dynamic.init_array_size = value,
                elf::DT_FINI_ARRAYSZ => dynamic.fini_array_size = value,
                elf::DT_DTPMOD64
                | elf::DT_DTPOFF64
                | elf::DT_TPOFF64
                | elf::DT_DTPMOD32
                | elf::DT_DTPOFF32
                | elf::DT_TPOFF32 => dynamic.unsupported_relocation = Some(tag),
                _ => {}
            }
        }
        Ok(dynamic)
    }
}

/// The constructor and destructor addresses the table names, in the order the C runtime would run
/// them: `DT_INIT` before `.init_array`, and `.fini_array` reversed before `DT_FINI`.
pub(super) fn lifecycle(
    mapping: &Mapping,
    dynamic: &Dynamic,
) -> Result<(Vec<usize>, Vec<usize>), String> {
    let mut initializers = Vec::new();
    if let Some(address) = dynamic.init {
        initializers.push(mapping.address(address, "DT_INIT")?);
    }
    initializers.extend(read_array(
        mapping,
        dynamic.init_array,
        dynamic.init_array_size,
        "DT_INIT_ARRAY",
    )?);
    let mut finalizers = read_array(
        mapping,
        dynamic.fini_array,
        dynamic.fini_array_size,
        "DT_FINI_ARRAY",
    )?;
    finalizers.reverse();
    if let Some(address) = dynamic.fini {
        finalizers.push(mapping.address(address, "DT_FINI")?);
    }
    Ok((initializers, finalizers))
}

/// Read one function-pointer array, skipping the null and `-1` slots a linker leaves behind.
fn read_array(
    mapping: &Mapping,
    address: Option<u64>,
    size: u64,
    what: &str,
) -> Result<Vec<usize>, String> {
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
    if !mapping.contains(address, size) {
        return Err(format!("MC {what} lies outside a loadable segment"));
    }
    let start = mapping.address(address, what)?;
    let mut functions = Vec::with_capacity((size / 8) as usize);
    for index in 0..(size / 8) as usize {
        let value = unsafe { (start as *const usize).add(index).read_unaligned() };
        if value != 0 && value != usize::MAX {
            functions.push(value);
        }
    }
    Ok(functions)
}
