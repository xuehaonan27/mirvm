//! Self-parsing an ELF64 image: the file header, the program headers the mapping phase walks, and
//! the section headers the other phases look their sections up in.
//!
//! A read that fails returns the loader's generic sentence rather than saying which field was
//! missing: a shape this format does not define is the same top-level failure whether it was the
//! magic, an entry size or a field past the end of the bytes.

use crate::native::object::elf;
use crate::utils::bytes::{read_u32, read_u64};

use super::bad;

/// One program header: the segment's file range, its virtual range and its access flags.
#[derive(Clone, Copy, Debug)]
pub(super) struct ProgramHeader {
    pub ty: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub flags: u32,
    /// Read so a truncated header is still rejected; the mapping rounds with its own page
    /// granularity rather than with `p_align`.
    #[allow(dead_code)]
    pub align: u64,
}

/// The sections the loader looks for, by type for the symbol tables and by name for the string
/// table and the unwind records. A scan keeps the last match, which is the section a linker
/// resolves a duplicate name against.
#[derive(Default)]
pub(super) struct Tables {
    pub symtab: Option<elf::Section>,
    pub dynsym: Option<elf::Section>,
    pub strtab: Option<elf::Section>,
    pub eh_frame: Option<elf::Section>,
}

/// An ELF64 image read from its bytes.
pub(super) struct Image<'a> {
    pub bytes: &'a [u8],
    pub program_headers: Box<[ProgramHeader]>,
    sections: Box<[elf::Section]>,
    shstrndx: usize,
}

impl<'a> Image<'a> {
    /// Read the headers, rejecting a shape this loader does not handle: another file type,
    /// another machine, or a program-header entry size smaller than the format defines.
    pub(super) fn parse(bytes: &'a [u8]) -> Result<Self, crate::error::Error> {
        let header = elf::FileHeader::parse(bytes)
            .ok_or_else(bad)
            .map_err(|e| crate::fail!(Native, e))?;
        if header.kind != elf::ET_DYN {
            return Err(crate::fail!(
                Native,
                "MC image is not ET_DYN (self-produced family should be a shared object)"
            ));
        }
        if header.machine != crate::arch::ELF_MACHINE {
            return Err(crate::fail!(
                Native,
                format!(
                    "MC image was built for ELF machine {} but this host is {}",
                    header.machine,
                    crate::arch::ELF_MACHINE
                )
            ));
        }
        let phoff = usize::try_from(header.phoff)
            .map_err(|_| bad())
            .map_err(|e| crate::fail!(Native, e))?;
        let phentsize = usize::from(header.phentsize);
        if phentsize < elf::PHDR_SIZE {
            return Err(crate::fail!(Native, bad()));
        }
        let sections = elf::sections(bytes, &header)
            .ok_or_else(bad)
            .map_err(|e| crate::fail!(Native, e))?;
        let mut program_headers = Vec::with_capacity(usize::from(header.phnum));
        for index in 0..usize::from(header.phnum) {
            program_headers.push(
                program_header_at(bytes, phoff, phentsize, index)
                    .ok_or_else(bad)
                    .map_err(|e| crate::fail!(Native, e))?,
            );
        }
        Ok(Self {
            bytes,
            program_headers: program_headers.into_boxed_slice(),
            sections: sections.into_boxed_slice(),
            shstrndx: usize::from(header.shstrndx),
        })
    }

    /// The section at `index`, which is how a symbol table names its string table through
    /// `sh_link`.
    pub(super) fn section_at(&self, index: usize) -> Option<elf::Section> {
        self.sections.get(index).copied()
    }

    /// The section-header string table, which names every section.
    pub(super) fn section_names(&self) -> Result<elf::Section, crate::error::Error> {
        self.section_at(self.shstrndx)
            .ok_or_else(|| crate::fail!(Native, bad()))
    }

    /// Find the sections the other phases work on, compared against `names`.
    pub(super) fn tables(&self, names: elf::Section) -> Tables {
        let mut tables = Tables::default();
        for section in self.sections.iter() {
            match (section.ty, self.section_name(&names, section)) {
                (elf::SHT_SYMTAB, _) => tables.symtab = Some(*section),
                (elf::SHT_DYNSYM, _) => tables.dynsym = Some(*section),
                (elf::SHT_STRTAB, ".strtab") => tables.strtab = Some(*section),
                (_, ".eh_frame") => tables.eh_frame = Some(*section),
                _ => {}
            }
        }
        tables
    }

    /// The NUL-terminated name `section` carries in the section-header string table. A section
    /// whose name is not UTF-8 is unnamed, which no lookup here matches.
    ///
    /// A name whose offset falls outside the image is unnamed for the same reason: the section
    /// simply never matches, and the phase that required it reports an unsupported image instead
    /// of reading past the bytes.
    fn section_name<'s>(&'s self, names: &elf::Section, section: &elf::Section) -> &'s str {
        let start = (names.offset + u64::from(section.name)) as usize;
        let Some(tail) = self.bytes.get(start..) else {
            return "";
        };
        let end = tail
            .iter()
            .position(|&c| c == 0)
            .map(|position| start + position)
            .unwrap_or(start);
        std::str::from_utf8(&self.bytes[start..end]).unwrap_or("")
    }
}

fn program_header_at(
    bytes: &[u8],
    phoff: usize,
    phentsize: usize,
    index: usize,
) -> Option<ProgramHeader> {
    let base = phoff.checked_add(index.checked_mul(phentsize)?)?;
    Some(ProgramHeader {
        ty: read_u32(bytes, base + elf::phdr::TYPE)?,
        flags: read_u32(bytes, base + elf::phdr::FLAGS)?,
        offset: read_u64(bytes, base + elf::phdr::OFFSET)?,
        vaddr: read_u64(bytes, base + elf::phdr::VADDR)?,
        filesz: read_u64(bytes, base + elf::phdr::FILESZ)?,
        memsz: read_u64(bytes, base + elf::phdr::MEMSZ)?,
        align: read_u64(bytes, base + elf::phdr::ALIGN)?,
    })
}
