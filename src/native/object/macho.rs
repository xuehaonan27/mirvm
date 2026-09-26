//! The 64-bit Mach-O container: what its bytes say about the file.
//!
//! The header, the load commands and the sections, read as a layout and nothing more: a reader
//! answers what the image holds and never what to do about it. The constants that name the format
//! (the magic, the sizes, the command numbers, the meaning of a section's type and of a symbol's
//! bits) live here so that one file states what these bytes are.
//!
//! Writing an image is not here: the symbol image mirvm hands a symbolizer is a real Mach-O its
//! own writer builds, and that writer is `crate::native::symbol::image`. Reading names out of an
//! image's symbol table is `crate::native::symbol::symtab`'s. What is left here is shared by both.

use crate::utils::bytes::{read_u32, read_u64};

/// The 64-bit Mach-O magic, little-endian on disk.
pub const MH_MAGIC_64: u32 = 0xfeed_facf;
/// `mach_header_64`, fixed part.
pub const HEADER_SIZE: usize = 32;
/// `segment_command_64`, without its section headers.
pub const SEGMENT_COMMAND_SIZE: usize = 72;
/// `section_64`.
pub const SECTION_SIZE: usize = 80;
/// One `nlist_64`.
pub const NLIST_SIZE: usize = 16;
/// The load command that carries `__TEXT` and its `__text` section.
pub const LC_SEGMENT_64: u32 = 0x19;
/// The load command carrying the singular constructor address in 64-bit images.
pub const LC_ROUTINES_64: u32 = 0x1a;
/// The load command that locates the image's symbol and string tables.
pub const LC_SYMTAB: u32 = 0x02;
/// `n_type` for a symbol defined at a section offset and visible to other images.
pub const N_SECT: u8 = 0x0e;
pub const N_EXT: u8 = 0x01;
/// `n_desc`'s bit for a weak definition.
pub const N_WEAK_DEF: u16 = 0x0080;
/// The low byte of `section_64.flags`, which is the section's type. The two types a pointer array
/// of functions can have, the type a modern ld64 gives the initializer list instead of one of
/// those, and the plain type that is none of them: a loader runs the lists it sees typed, so a
/// section whose type stops saying "a list of initializers" is one it leaves alone.
pub const SECTION_TYPE_MASK: u32 = 0xff;
pub const S_MOD_INIT_FUNC_POINTERS: u32 = 0x9;
pub const S_MOD_TERM_FUNC_POINTERS: u32 = 0xa;
pub const S_INIT_FUNC_OFFSETS: u32 = 0x16;
pub const S_REGULAR: u32 = 0x0;
/// `VM_PROT_EXECUTE`, as `segment_command_64.initprot` reports it.
pub const VM_PROT_EXECUTE: u32 = 0x4;
/// `n_type`'s type field, the value meaning "defined nowhere in this image", and the bit marking a
/// symbol the image defines but does not export.
pub const N_TYPE: u8 = 0x0e;
pub const N_UNDF: u8 = 0x00;
pub const N_PEXT: u8 = 0x10;

/// One `LC_SEGMENT_64`: the virtual range it maps, how it is protected, and the sections it holds.
pub(crate) struct Segment {
    pub(crate) vmaddr: u64,
    pub(crate) vmsize: u64,
    pub(crate) initprot: u32,
    pub(crate) sections: Vec<Section>,
}

/// One `section_64`: the two names a caller matches it by, its virtual range, and its type.
pub(crate) struct Section {
    pub(crate) segname: String,
    pub(crate) sectname: String,
    pub(crate) addr: u64,
    pub(crate) size: u64,
    pub(crate) section_type: u32,
    /// Where the type lives in the load commands, which is where clearing it is written.
    pub(crate) flags_at: usize,
}

impl Segment {
    /// Whether this segment maps executable code, which is what a caller attributes instruction
    /// addresses to.
    pub(crate) fn is_executable(&self) -> bool {
        self.initprot & VM_PROT_EXECUTE != 0
    }
}

/// What walking an image's load commands yields a caller.
pub(crate) struct Image {
    pub(crate) segments: Vec<Segment>,
    /// The singular constructor address an `LC_ROUTINES_64` carries, which this linker does not
    /// emit but an older image may have.
    pub(crate) routines_init: Option<u64>,
}

/// The image's `LC_SEGMENT_64` commands, in the order the header lists them.
pub(crate) fn image(bytes: &[u8]) -> Result<Image, String> {
    let (ncmds, sizeofcmds) = header(bytes)?;
    let commands_end = HEADER_SIZE
        .checked_add(sizeofcmds as usize)
        .ok_or_else(|| "the image's load commands are too long".to_string())?;
    if commands_end > bytes.len() {
        return Err("the image ends inside its load commands".to_string());
    }
    let mut segments = Vec::new();
    let mut routines_init = None;
    let mut cursor = HEADER_SIZE;
    for _ in 0..ncmds {
        let (command, cmdsize) = load_command(bytes, cursor)?;
        let size = cmdsize as usize;
        // Every command is at least its own header and the header's count and total size are the
        // only bounds on the run.
        if size < 8 || cursor + size > commands_end {
            return Err("a load command has an implausible size".to_string());
        }
        if command == LC_SEGMENT_64 {
            segments.push(segment(bytes, cursor, size)?);
        } else if command == LC_ROUTINES_64 {
            if size < 16 {
                return Err("an LC_ROUTINES_64 is truncated".to_string());
            }
            routines_init = read_u64(bytes, cursor + 8);
        }
        cursor += size;
    }
    Ok(Image {
        segments,
        routines_init,
    })
}

fn segment(bytes: &[u8], cursor: usize, cmdsize: usize) -> Result<Segment, String> {
    let short = || "a segment command is truncated".to_string();
    if cmdsize < SEGMENT_COMMAND_SIZE {
        return Err(short());
    }
    let vmaddr = read_u64(bytes, cursor + 24).ok_or_else(short)?;
    let vmsize = read_u64(bytes, cursor + 32).ok_or_else(short)?;
    let initprot = read_u32(bytes, cursor + 60).ok_or_else(short)?;
    let nsects = read_u32(bytes, cursor + 64).ok_or_else(short)? as usize;
    let sections_start = cursor + SEGMENT_COMMAND_SIZE;
    let sections_size = nsects
        .checked_mul(SECTION_SIZE)
        .ok_or_else(|| "a segment declares too many sections".to_string())?;
    if sections_start + sections_size > cursor + cmdsize {
        return Err(short());
    }
    let mut sections = Vec::with_capacity(nsects);
    for index in 0..nsects {
        sections.push(section(bytes, sections_start + index * SECTION_SIZE)?);
    }
    Ok(Segment {
        vmaddr,
        vmsize,
        initprot,
        sections,
    })
}

fn section(bytes: &[u8], at: usize) -> Result<Section, String> {
    let short = || "a section header is truncated".to_string();
    Ok(Section {
        sectname: fixed_name(bytes, at).ok_or_else(short)?,
        segname: fixed_name(bytes, at + 16).ok_or_else(short)?,
        addr: read_u64(bytes, at + 32).ok_or_else(short)?,
        size: read_u64(bytes, at + 40).ok_or_else(short)?,
        section_type: read_u32(bytes, at + 64).ok_or_else(short)? & SECTION_TYPE_MASK,
        flags_at: at + 64,
    })
}

/// A `section_64`'s 16-byte name field, NUL-padded, as a string.
fn fixed_name(bytes: &[u8], at: usize) -> Option<String> {
    let field = bytes.get(at..at + 16)?;
    let end = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(field.len());
    Some(String::from_utf8_lossy(&field[..end]).into_owned())
}

/// Whether `bytes` is a 64-bit Mach-O image at all, which is what a caller holding a member of an
/// archive asks before reading symbols out of it.
pub(crate) fn is_image(bytes: &[u8]) -> bool {
    read_u32(bytes, 0) == Some(MH_MAGIC_64)
}

/// The image's `ncmds` and `sizeofcmds`.
pub(crate) fn header(bytes: &[u8]) -> Result<(u32, u32), String> {
    match read_u32(bytes, 0) {
        Some(MH_MAGIC_64) => {}
        _ => return Err("not a 64-bit Mach-O image".to_string()),
    }
    let short = || "the image ends inside its header".to_string();
    Ok((
        read_u32(bytes, 16).ok_or_else(short)?,
        read_u32(bytes, 20).ok_or_else(short)?,
    ))
}

/// A load command's `cmd` and `cmdsize`.
pub(crate) fn load_command(bytes: &[u8], cursor: usize) -> Result<(u32, u32), String> {
    let short = || "the image ends inside a load command".to_string();
    Ok((
        read_u32(bytes, cursor).ok_or_else(short)?,
        read_u32(bytes, cursor + 4).ok_or_else(short)?,
    ))
}
