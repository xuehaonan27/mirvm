//! Self-parsing an ELF64 image: the file header, the program headers the mapping phase walks, and
//! the section headers the other phases look their sections up in.
//!
//! A read that fails returns the loader's generic sentence rather than saying which field was
//! missing: a shape this format does not define is the same top-level failure whether it was the
//! magic, an entry size or a field past the end of the bytes.

use crate::native::elf;

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
    pub(super) fn parse(bytes: &'a [u8]) -> Result<Self, String> {
        let header = elf::FileHeader::parse(bytes).ok_or_else(bad)?;
        if header.kind != elf::ET_DYN {
            return Err(
                "MC image is not ET_DYN (self-produced family should be a shared object)".into(),
            );
        }
        if header.machine != crate::arch::ELF_MACHINE {
            return Err(format!(
                "MC image was built for ELF machine {} but this host is {}",
                header.machine,
                crate::arch::ELF_MACHINE
            ));
        }
        let phoff = usize::try_from(header.phoff).map_err(|_| bad())?;
        let phentsize = usize::from(header.phentsize);
        if phentsize < elf::PHDR_SIZE {
            return Err(bad());
        }
        let sections = elf::sections(bytes, &header).ok_or_else(bad)?;
        let mut program_headers = Vec::with_capacity(usize::from(header.phnum));
        for index in 0..usize::from(header.phnum) {
            program_headers
                .push(program_header_at(bytes, phoff, phentsize, index).ok_or_else(bad)?);
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
    pub(super) fn section_names(&self) -> Result<elf::Section, String> {
        self.section_at(self.shstrndx).ok_or_else(bad)
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
    fn section_name<'s>(&'s self, names: &elf::Section, section: &elf::Section) -> &'s str {
        let start = (names.offset + u64::from(section.name)) as usize;
        let end = self.bytes[start..]
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
        ty: elf::u32_at(bytes, base + elf::phdr::TYPE)?,
        flags: elf::u32_at(bytes, base + elf::phdr::FLAGS)?,
        offset: elf::u64_at(bytes, base + elf::phdr::OFFSET)?,
        vaddr: elf::u64_at(bytes, base + elf::phdr::VADDR)?,
        filesz: elf::u64_at(bytes, base + elf::phdr::FILESZ)?,
        memsz: elf::u64_at(bytes, base + elf::phdr::MEMSZ)?,
        align: elf::u64_at(bytes, base + elf::phdr::ALIGN)?,
    })
}
