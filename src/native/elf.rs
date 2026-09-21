//! The ELF64 little-endian byte layout.
//!
//! Every reader answers `None` for truncation or for a field that is not the one this format
//! defines, and never decides what a failure means: a caller maps `None` to the error its own
//! layer reports, which is why a malformed image produces the loader's sentence and not this
//! module's. Which machine an image is for is the CPU's (`arch::ELF_MACHINE`); which format a
//! host's loader accepts, and everything the loader does with the bytes, is `os`'s.

/// The four bytes every ELF image begins with.
pub const IDENT: [u8; 4] = [0x7f, b'E', b'L', b'F'];
/// `EI_CLASS` for 64-bit.
pub const ELFCLASS64: u8 = 2;
/// `EI_DATA` for little-endian.
pub const ELFDATA2LSB: u8 = 1;

/// File header size, and the byte count a header parse needs.
pub const EHDR_SIZE: usize = 64;
/// Program header size.
pub const PHDR_SIZE: usize = 56;
/// Section header size.
pub const SHDR_SIZE: usize = 64;
/// Symbol table entry size.
pub const SYM_ENTRY_SIZE: usize = 24;

/// `e_type`: a shared object.
pub const ET_DYN: u16 = 3;

/// `p_type` values the self-loader acts on.
pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_INTERP: u32 = 3;

/// `sh_type` values the symbol readers act on.
pub const SHT_SYMTAB: u32 = 2;
pub const SHT_DYNSYM: u32 = 11;

/// `st_shndx`: the symbol is undefined in this image.
pub const SHN_UNDEF: u16 = 0;
/// `st_shndx`: the first reserved (processor- or OS-specific) index; below it is a real section.
pub const SHN_RESERVED: u16 = 0xff00;

/// `st_info >> 4`: the bindings a static symbol enumeration considers.
pub const STB_GLOBAL: u8 = 1;
pub const STB_WEAK: u8 = 2;

/// File header field offsets, named for the writer that assembles a header field by field as well
/// as for the reader below.
pub mod ehdr {
    pub const TYPE: usize = 16;
    pub const MACHINE: usize = 18;
    pub const PHOFF: usize = 32;
    pub const SHOFF: usize = 40;
    pub const PHENTSIZE: usize = 54;
    pub const PHNUM: usize = 56;
    pub const SHENTSIZE: usize = 58;
    pub const SHNUM: usize = 60;
    pub const SHSTRNDX: usize = 62;
}

/// Little-endian readers. `None` when the field runs past the end of `bytes`.
pub fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

pub fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

pub fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// Little-endian writers. The caller owns the layout it is assembling, so an offset it computed
/// out of range is a bug in that layout and panics here rather than being silently dropped.
pub fn put_u16(out: &mut [u8], offset: usize, value: u16) {
    out[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

pub fn put_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn put_u64(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// Whether `bytes` is an ELF64 little-endian image. The header past these first six bytes may still
/// be truncated, which [`FileHeader::parse`] rejects.
pub fn is_elf64_le(bytes: &[u8]) -> bool {
    bytes.len() >= EHDR_SIZE
        && bytes[..4] == IDENT
        && bytes[4] == ELFCLASS64
        && bytes[5] == ELFDATA2LSB
}

/// The ELF64 little-endian file header.
#[derive(Clone, Copy, Debug)]
pub struct FileHeader {
    pub kind: u16,
    pub machine: u16,
    pub phoff: u64,
    pub shoff: u64,
    pub phentsize: u16,
    pub phnum: u16,
    pub shentsize: u16,
    pub shnum: u16,
    pub shstrndx: u16,
}

impl FileHeader {
    /// Read the header of an ELF64 LE image, or `None` when it is not one or a field lies past the
    /// end. What counts as an acceptable `kind`, `machine` or entry size is the caller's question,
    /// because different callers accept different images.
    pub fn parse(bytes: &[u8]) -> Option<FileHeader> {
        if !is_elf64_le(bytes) {
            return None;
        }
        Some(FileHeader {
            kind: u16_at(bytes, ehdr::TYPE)?,
            machine: u16_at(bytes, ehdr::MACHINE)?,
            phoff: u64_at(bytes, ehdr::PHOFF)?,
            shoff: u64_at(bytes, ehdr::SHOFF)?,
            phentsize: u16_at(bytes, ehdr::PHENTSIZE)?,
            phnum: u16_at(bytes, ehdr::PHNUM)?,
            shentsize: u16_at(bytes, ehdr::SHENTSIZE)?,
            shnum: u16_at(bytes, ehdr::SHNUM)?,
            shstrndx: u16_at(bytes, ehdr::SHSTRNDX)?,
        })
    }
}

/// An ELF64 section header: the fields its readers use, which is why the layout's `sh_flags`,
/// `sh_info` and `sh_addralign` are not read here. Add them when a reader needs them.
#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub name: u32,
    pub ty: u32,
    pub addr: u64,
    pub offset: u64,
    pub size: u64,
    pub link: u32,
    pub entsize: u64,
}

/// The section header table. `e_shnum == 0` is the SHN_UNDEF extension: the real count moved into
/// section 0's `size`, because a count above 0xff00 no longer fits `e_shnum`. `None` when the entry
/// size is not a section header or any entry lies past the end.
pub fn sections(bytes: &[u8], header: &FileHeader) -> Option<Vec<Section>> {
    if usize::from(header.shentsize) < SHDR_SIZE {
        return None;
    }
    let mut count = usize::from(header.shnum);
    if count == 0 {
        count = usize::try_from(section_at(bytes, header, 0)?.size).ok()?;
    }
    (0..count).map(|i| section_at(bytes, header, i)).collect()
}

fn section_at(bytes: &[u8], header: &FileHeader, index: usize) -> Option<Section> {
    let base = usize::try_from(header.shoff)
        .ok()?
        .checked_add(index.checked_mul(usize::from(header.shentsize))?)?;
    Some(Section {
        name: u32_at(bytes, base)?,
        ty: u32_at(bytes, base + 4)?,
        addr: u64_at(bytes, base + 16)?,
        offset: u64_at(bytes, base + 24)?,
        size: u64_at(bytes, base + 32)?,
        link: u32_at(bytes, base + 40)?,
        entsize: u64_at(bytes, base + 56)?,
    })
}
