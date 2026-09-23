//! An arm64 Mach-O dylib whose only content is a symbol per guest function.
//!
//! A process symbolizer discovers the images a process has loaded and reads their own symbol
//! tables, so the way to make a synthetic instruction-pointer token resolvable is to hand it a
//! real, loadable image that defines a symbol at that address. On macOS that image is a dylib, and
//! arm64 macOS refuses to `dlopen` an unsigned one, so [`build`] emits the unsigned bytes and a
//! caller ad-hoc signs them (`codesign -s - --force`), which appends the signature.
//!
//! The image is one inert slot per function, `SLOT` bytes apart, so an address is always a slot
//! start and never mid-instruction. Nothing ever executes these bytes; their addresses are opaque
//! tokens. `__TEXT` is laid out with `vmaddr` 0 and `fileoff` 0, which makes a slot's file offset
//! and its virtual address the same number; `build` returns the slot region's file offset, so a
//! caller turns index `i` into a runtime address as `load_bias + text_off + i * SLOT`, where
//! `load_bias` is dyld's slide plus `__TEXT`'s vmaddr.

/// The 64-bit Mach-O magic, little-endian on disk.
const MH_MAGIC_64: u32 = 0xfeed_facf;
/// `cputype` for arm64, and the subtype meaning "all arm64".
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_SUBTYPE_ARM64_ALL: u32 = 0;
/// `filetype`: a dynamically linked shared library.
const MH_DYLIB: u32 = 6;
/// The header flags: `MH_NOUNDEFS`, `MH_DYLDLINK`, `MH_TWOLEVEL` and `MH_PIE`. A dylib with no
/// dependency list carries all four, and they let the tools that walk an image's load commands
/// treat it as a linkable library rather than a bundle.
const MH_FLAGS: u32 = 0x1 | 0x4 | 0x80 | 0x0020_0000;

/// `mach_header_64`, fixed part.
const HEADER_SIZE: usize = 32;
/// `segment_command_64`, without its section headers.
const SEGMENT_COMMAND_SIZE: usize = 72;
/// `section_64`.
const SECTION_SIZE: usize = 80;
/// `LC_UUID`'s command and 16 UUID bytes.
const UUID_COMMAND_SIZE: usize = 24;
/// `symtab_command`.
const SYMTAB_COMMAND_SIZE: usize = 24;
/// `dysymtab_command`.
const DYSYMTAB_COMMAND_SIZE: usize = 80;
/// `dylib_command` without its name.
const DYLIB_COMMAND_SIZE: usize = 24;
/// One `nlist_64`.
const NLIST_SIZE: usize = 16;

/// The load command that carries `__TEXT` and its `__text` section.
const LC_SEGMENT_64: u32 = 0x19;
/// The load command naming this image, and the one dyld needs to tell two loads of it apart.
const LC_ID_DYLIB: u32 = 0x0d;
const LC_UUID: u32 = 0x1b;
/// The two symbol-table commands.
const LC_SYMTAB: u32 = 0x02;
const LC_DYSYMTAB: u32 = 0x0b;

/// `__TEXT`'s permissions (read and execute) and `__LINKEDIT`'s (read only).
const PROT_READ: u32 = 1;
const PROT_EXECUTE: u32 = 4;

/// `n_type` for a symbol defined at a section offset and visible to other images.
const N_SECT: u8 = 0x0e;
const N_EXT: u8 = 0x01;

/// `S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS` on the `__text` section.
const TEXT_SECTION_FLAGS: u32 = 0x8000_0400;
/// The `__text` section is 2^4 bytes aligned, which is what a `SLOT`-byte stride needs.
const SLOT_ALIGN_LOG2: u32 = 4;

/// The section index of `__text` within `__TEXT`, which is the only section in the image.
const TEXT_SECTION_INDEX: u8 = 1;
/// `__TEXT` starts at file offset zero and virtual address zero, so the two coincide and any
/// offset into the image is also the address the image maps it at.
const TEXT_SEGMENT_FILE_OFFSET: u64 = 0;
const TEXT_SEGMENT_VM_ADDR: u64 = 0;

/// The install name recorded in `LC_ID_DYLIB`. Nothing loads this image by name: a caller
/// `dlopen`s it by path, so the name only has to be a valid absolute path.
const INSTALL_NAME: &str = "/usr/lib/libmirvm_syms.dylib";

/// macOS on arm64 maps 16 KiB pages, and every segment offset and size here is a multiple of the
/// page size so that `codesign` can cover the segments and dyld can map them.
const PAGE: usize = 0x4000;

/// One function's slot: the aarch64 encoding of `ret` followed by `nop` padding, so a slot start is
/// a decodable instruction and nothing after the return is ever reached.
const SLOT: usize = 16;
const SLOT_BYTES: [u8; SLOT] = {
    let ret = 0xd65f_03c0_u32.to_le_bytes();
    let nop = 0xd503_201f_u32.to_le_bytes();
    [
        ret[0], ret[1], ret[2], ret[3], nop[0], nop[1], nop[2], nop[3], nop[0], nop[1], nop[2],
        nop[3], nop[0], nop[1], nop[2], nop[3],
    ]
};

fn align(value: usize, alignment: usize) -> usize {
    value.div_ceil(alignment) * alignment
}

/// Appends `value` and a terminating NUL, returning the offset the name starts at. The symbol
/// table's first byte is a NUL, so offset zero stays the empty name.
fn push_cstr(table: &mut Vec<u8>, value: &[u8]) -> Result<u32, String> {
    if value.contains(&0) {
        return Err("guest function name contains NUL".into());
    }
    let offset = u32::try_from(table.len()).map_err(|_| "Mach-O string table too large")?;
    table.extend_from_slice(value);
    table.push(0);
    Ok(offset)
}

/// A cursor over the growing image. The laid-out offsets of the header, the segments and the
/// linkedit data all land on multiples of eight, so bounding each write by the image length is
/// enough to keep one field from overwriting the next.
struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes the 16-byte `segname`/`sectname` fields, which are NUL-padded and never full.
    fn name(&mut self, value: &str) {
        let mut field = [0u8; 16];
        field[..value.len()].copy_from_slice(value.as_bytes());
        self.out.extend_from_slice(&field);
    }

    /// Writes `string` and pads to an 8-byte boundary. All three uses here (`LC_ID_DYLIB`'s name
    /// and the two `LC_SEGMENT_64` names) already sit at a multiple of eight.
    fn string(&mut self, string: &str) {
        self.out.extend_from_slice(string.as_bytes());
        self.out.push(0);
        while self.out.len() % 8 != 0 {
            self.out.push(0);
        }
    }
}

/// The sixteen bytes `LC_UUID` carries. dyld only needs the value to differ between two images it
/// is asked to load, and a symbolizer only needs it to name one; hashing the layout gives that
/// without a source of randomness.
fn uuid(text_off: usize, slot_count: usize) -> [u8; 16] {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in (text_off as u64)
        .to_le_bytes()
        .into_iter()
        .chain((slot_count as u64).to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&hash.to_le_bytes());
    // The low half repeats the hash with a different odd multiplier, which is enough to fill the
    // command without another state word to keep track of.
    id[8..].copy_from_slice(&hash.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
    id
}

/// Builds the unsigned dylib and returns its bytes together with the file offset the slots start
/// at. The caller signs the bytes before loading them.
pub fn build(names: &[Box<str>]) -> Result<(Vec<u8>, usize), String> {
    let mut strtab = vec![0];
    let mut name_offsets = Vec::with_capacity(names.len());
    for name in names {
        name_offsets.push(push_cstr(&mut strtab, name.as_bytes())?);
    }

    let id_dylib_len = align(DYLIB_COMMAND_SIZE + INSTALL_NAME.len() + 1, 8);
    let segment_len = SEGMENT_COMMAND_SIZE + SECTION_SIZE;
    // Header, then `LC_ID_DYLIB`, `LC_SEGMENT_64` for `__TEXT`, `LC_SEGMENT_64` for `__LINKEDIT`,
    // `LC_UUID`, `LC_SYMTAB` and `LC_DYSYMTAB`, in that order. `LC_CODE_SIGNATURE` is deliberately
    // absent: `codesign` appends the command with the signature, while an image that carries the
    // command and no signature is one `codesign` rejects as malformed instead of signing.
    let commands_len = id_dylib_len
        + segment_len
        + SEGMENT_COMMAND_SIZE
        + UUID_COMMAND_SIZE
        + SYMTAB_COMMAND_SIZE
        + DYSYMTAB_COMMAND_SIZE;
    // The slot region starts sixteen bytes past the first 16-byte boundary at or after the load
    // commands. The zero bytes between the two lie outside `sizeofcmds` and outside every load
    // command: they are the room `codesign` inserts `LC_CODE_SIGNATURE` into. They also make the
    // section offset larger than the header plus `sizeofcmds`, which readers require of a section
    // in a segment that starts at file offset zero.
    let text_off = align(HEADER_SIZE + commands_len, SLOT) + SLOT;
    let text_len = names
        .len()
        .checked_mul(SLOT)
        .ok_or("Mach-O text section too large")?;
    let text_end = text_off + text_len;
    // `__LINKEDIT` has to begin on a page boundary, and the only thing between it and the slots is
    // the padding that brings the text segment up to the page it occupies. `codesign` reads this
    // padding as part of `__TEXT` and covers it; `dyld` never maps it as anything.
    let sym_off = align(text_end, PAGE);
    let sym_len = names
        .len()
        .checked_mul(NLIST_SIZE)
        .ok_or("Mach-O symbol table too large")?;
    let str_off = sym_off + sym_len;
    // The string table ends the file, and its length is not padded: dyld reads entries by offset,
    // and the next thing on disk is the signature `codesign` appends, which it aligns itself. The
    // symbol table is linkedit data, so `__LINKEDIT` covers the run between the two offsets.
    let file_len = str_off + strtab.len();
    let linkedit_len = file_len - sym_off;
    let linkedit_vmsize = align(linkedit_len, PAGE);
    // `__TEXT` carries the header, the commands and the slots, and its file and virtual sizes are
    // both the page it occupies, so the padding above is inside the segment on disk as well as in
    // memory.
    let text_filesize = sym_off;
    let text_vmsize = sym_off;

    let mut out = Writer {
        out: Vec::with_capacity(file_len),
    };

    let command_count = 6u32;
    // mach_header_64
    out.u32(MH_MAGIC_64);
    out.u32(CPU_TYPE_ARM64);
    out.u32(CPU_SUBTYPE_ARM64_ALL);
    out.u32(MH_DYLIB);
    out.u32(command_count);
    out.u32(commands_len as u32);
    out.u32(MH_FLAGS); // flags
    out.u32(0); // reserved

    // LC_ID_DYLIB
    out.u32(LC_ID_DYLIB);
    out.u32(id_dylib_len as u32);
    out.u32(DYLIB_COMMAND_SIZE as u32); // dylib.name.offset: the name follows the command struct
    out.u32(0); // timestamp
    out.u32(0); // current_version
    out.u32(0); // compatibility_version
    out.string(INSTALL_NAME);

    // LC_SEGMENT_64 for __TEXT, with its one __text section. The section's offset and address are
    // both the slot region's file offset, which is `vmaddr`'s zero plus that offset.
    out.u32(LC_SEGMENT_64);
    out.u32(segment_len as u32);
    out.name("__TEXT");
    out.u64(TEXT_SEGMENT_VM_ADDR);
    out.u64(text_vmsize as u64);
    out.u64(TEXT_SEGMENT_FILE_OFFSET);
    out.u64(text_filesize as u64);
    out.u32(PROT_READ | PROT_EXECUTE); // maxprot
    out.u32(PROT_READ | PROT_EXECUTE); // initprot
    out.u32(1); // nsects
    out.u32(0); // flags
    out.name("__text");
    out.name("__TEXT");
    out.u64(text_off as u64); // addr
    out.u64(text_len as u64);
    out.u32(text_off as u32); // offset
    out.u32(SLOT_ALIGN_LOG2);
    out.u32(0); // reloff
    out.u32(0); // nreloc
    out.u32(TEXT_SECTION_FLAGS);
    out.u32(0); // reserved1
    out.u32(0); // reserved2
    out.u32(0); // reserved3

    // LC_SEGMENT_64 for __LINKEDIT, which covers the symbol and string tables. Its file offset and
    // address coincide for the same reason `__TEXT`'s do, and its virtual size is what makes the
    // segment the page after the text.
    out.u32(LC_SEGMENT_64);
    out.u32(SEGMENT_COMMAND_SIZE as u32);
    out.name("__LINKEDIT");
    out.u64(sym_off as u64); // vmaddr
    out.u64(linkedit_vmsize as u64);
    out.u64(sym_off as u64); // fileoff
    out.u64(linkedit_len as u64);
    out.u32(PROT_READ); // maxprot
    out.u32(PROT_READ); // initprot
    out.u32(0); // nsects
    out.u32(0); // flags

    // LC_UUID
    out.u32(LC_UUID);
    out.u32(UUID_COMMAND_SIZE as u32);
    out.out.extend_from_slice(&uuid(text_off, names.len()));

    // LC_SYMTAB
    out.u32(LC_SYMTAB);
    out.u32(SYMTAB_COMMAND_SIZE as u32);
    out.u32(sym_off as u32); // symoff
    out.u32(names.len() as u32); // nsyms
    out.u32(str_off as u32); // stroff
    out.u32(strtab.len() as u32); // strsize

    // LC_DYSYMTAB. Every symbol is external and defined, so the external-defined range is the
    // whole table and the local and undefined ranges are empty. This is the classification that
    // makes `nm` and the symbolizers report the names.
    out.u32(LC_DYSYMTAB);
    out.u32(DYSYMTAB_COMMAND_SIZE as u32);
    out.u32(0); // ilocalsym
    out.u32(0); // nlocalsym
    out.u32(0); // iextdefsym
    out.u32(names.len() as u32); // nextdefsym
    out.u32(names.len() as u32); // iundefsym
    out.u32(0); // nundefsym
    // The remaining twelve fields describe tables this image does not carry: tocoff, ntoc,
    // modtaboff, nmodtab, extrefsymoff, nextrefsyms, indirectsymoff, nindirectsyms, extreloff,
    // nextrel, locreloff, nlocrel.
    for _ in 0..12 {
        out.u32(0);
    }

    debug_assert!(
        out.out.len() <= text_off,
        "load commands fit before the slots"
    );
    out.out
        .extend(std::iter::repeat_n(0u8, text_off - out.out.len()));

    for _ in &name_offsets {
        out.out.extend_from_slice(&SLOT_BYTES);
    }

    debug_assert_eq!(out.out.len(), text_end, "the slots end the text segment");
    out.out
        .extend(std::iter::repeat_n(0u8, sym_off - out.out.len()));

    // One nlist_64 per name: external, defined in `__text`, at the start of its slot. There are no
    // local and no undefined symbols, so writing them in index order is already the locals /
    // external-defined / undefined order the format requires.
    for (index, &name) in name_offsets.iter().enumerate() {
        out.u32(name); // n_strx
        out.out.push(N_SECT | N_EXT); // n_type
        out.out.push(TEXT_SECTION_INDEX); // n_sect
        out.out.extend_from_slice(&0u16.to_le_bytes()); // n_desc
        out.u64((text_off + index * SLOT) as u64); // n_value
    }

    out.out.extend_from_slice(&strtab);
    debug_assert_eq!(out.out.len(), file_len, "the string table ends the file");
    Ok((out.out, text_off))
}
