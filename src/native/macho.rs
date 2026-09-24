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
//!
//! Two tables carry the names, and both spell them the way this format spells a C symbol — with a
//! leading underscore, because a loader looks a C name up with one applied: the symbol table, which
//! `nm` and `dladdr` read, and the export trie, which is the only one `dlsym` consults.

use super::lifecycle::CallableList;

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
/// `dyld_info_command`, which carries the export trie's offset and size.
const DYLD_INFO_COMMAND_SIZE: usize = 48;
/// Every run of `__LINKEDIT` starts on this boundary. A loader and the tools that read an image
/// reject a blob that does not: `dyld_info` calls a trie that starts anywhere else mis-aligned.
const LINKEDIT_ALIGN: usize = 8;
/// `dylib_command` without its name.
const DYLIB_COMMAND_SIZE: usize = 24;
/// One `nlist_64`.
const NLIST_SIZE: usize = 16;

/// The load command that carries `__TEXT` and its `__text` section.
const LC_SEGMENT_64: u32 = 0x19;
/// The load command carrying the singular constructor address in 64-bit images.
const LC_ROUTINES_64: u32 = 0x1a;
/// The load command naming this image, and the one dyld needs to tell two loads of it apart.
const LC_ID_DYLIB: u32 = 0x0d;
const LC_UUID: u32 = 0x1b;
/// The two symbol-table commands, and the one that carries the export trie.
const LC_SYMTAB: u32 = 0x02;
const LC_DYSYMTAB: u32 = 0x0b;
const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;

/// The flags word of an export trie terminal: no flags, which is how an ordinary name is marked.
const EXPORT_SYMBOL_FLAGS_REGULAR: u64 = 0;

/// `__TEXT`'s permissions (read and execute) and `__LINKEDIT`'s (read only).
const PROT_READ: u32 = 1;
const PROT_EXECUTE: u32 = 4;

/// `n_type` for a symbol defined at a section offset and visible to other images.
const N_SECT: u8 = 0x0e;
const N_EXT: u8 = 0x01;
/// `n_desc`'s bit for a weak definition.
const N_WEAK_DEF: u16 = 0x0080;

/// The low byte of `section_64.flags`, which is the section's type. The two types a pointer array
/// of functions can have, the type a modern ld64 gives the initializer list instead of one of
/// those, and the plain type that is none of them: a loader runs the lists it sees typed, so a
/// section whose type stops saying "a list of initializers" is one it leaves alone.
const SECTION_TYPE_MASK: u32 = 0xff;
const S_MOD_INIT_FUNC_POINTERS: u32 = 0x9;
const S_MOD_TERM_FUNC_POINTERS: u32 = 0xa;
const S_INIT_FUNC_OFFSETS: u32 = 0x16;
const S_REGULAR: u32 = 0x0;

/// `VM_PROT_EXECUTE`, as `segment_command_64.initprot` reports it.
const VM_PROT_EXECUTE: u32 = 0x4;

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

/// One function's slot, which is this architecture's inert slot: a decodable instruction followed
/// by padding, so no address is ever mid-instruction and nothing after the return is reached.
const SLOT: usize = crate::arch::asmstub::INERT_SLOT.len();

/// One node of the export trie: where a name ends, and what continues from here.
#[derive(Default)]
struct Trie {
    /// The address of the symbol whose name ends exactly at this node.
    terminal: Option<u64>,
    /// What continues from this node, by the next byte of those names. Sorted, because a loader
    /// walks a node's edges in order.
    children: std::collections::BTreeMap<u8, Trie>,
}

/// A node of the trie as the layout sees it: its own bytes, and the children it points at by index
/// into the emit order.
struct TrieNode {
    terminal: Option<u64>,
    /// [`Self::terminal`]'s encoding plus the child-count byte plus one edge and terminator per
    /// child — everything whose length does not depend on where a child lands.
    own_len: usize,
    /// Each child's edge byte and its index, in the order it is emitted.
    children: Vec<(u8, usize)>,
}

/// The export trie that names every symbol of the image.
///
/// A loader resolves an exported name by walking this rather than by reading the symbol table, so
/// an image that carries symbols and no trie is one whose names nothing can look up — `dlsym` and
/// `dladdr` included.
///
/// Every node is emitted after its parent, in the byte order of the edges, and the offsets between
/// nodes are ULEB128, so a node's length depends on where its children land. The widths are settled
/// by starting from the widest encoding and narrowing until they stop changing: narrowing only ever
/// moves a child earlier, which can only narrow a width further, so the loop settles on widths that
/// agree with the offsets they encode.
fn export_trie(symbols: &[Vec<u8>], text_off: usize) -> Result<Vec<u8>, String> {
    let mut root = Trie::default();
    for (index, symbol) in symbols.iter().enumerate() {
        let mut node = &mut root;
        for byte in symbol {
            node = node.children.entry(*byte).or_default();
        }
        node.terminal = Some((text_off + index * SLOT) as u64);
    }

    // The emit order: a node, then its whole subtree, in edge order. Interning in that order makes
    // every child's index greater than its parent's, which is what lets the layout below walk the
    // arena once.
    fn intern(node: &Trie, arena: &mut Vec<TrieNode>) -> usize {
        let index = arena.len();
        arena.push(TrieNode {
            terminal: node.terminal,
            own_len: 0,
            children: Vec::new(),
        });
        let mut children = Vec::with_capacity(node.children.len());
        for (byte, child) in &node.children {
            children.push((*byte, intern(child, arena)));
        }
        // A terminal is `flags` then `address`, both ULEB128; a node without one writes a size of
        // zero and nothing after it. The size field is itself a ULEB128, which is why the length of
        // the field is counted rather than assumed to be one byte.
        let terminal_len = node
            .terminal
            .map(|address| uleb_len(EXPORT_SYMBOL_FLAGS_REGULAR) + uleb_len(address));
        let own_len = match terminal_len {
            Some(len) => uleb_len(len as u64) + len,
            None => 1,
        };
        // Then the child count, and one edge byte plus its terminator per child.
        arena[index] = TrieNode {
            terminal: node.terminal,
            own_len: own_len + 1 + children.len() * 2,
            children,
        };
        index
    }
    let mut arena = Vec::new();
    let root_index = intern(&root, &mut arena);
    debug_assert_eq!(root_index, 0, "the root is emitted first");

    let mut widths: Vec<Vec<usize>> = arena
        .iter()
        .map(|node| vec![ULEB_MAX; node.children.len()])
        .collect();
    let mut offsets = vec![0usize; arena.len()];
    loop {
        let mut at = 0;
        for (index, node) in arena.iter().enumerate() {
            offsets[index] = at;
            at += node.own_len + widths[index].iter().sum::<usize>();
        }
        let mut narrowed = widths.clone();
        for (index, node) in arena.iter().enumerate() {
            for (slot, &(_, child)) in node.children.iter().enumerate() {
                narrowed[index][slot] = uleb_len(offsets[child] as u64);
            }
        }
        if narrowed == widths {
            break;
        }
        widths = narrowed;
    }

    let mut out = Vec::with_capacity(offsets[0] + arena[0].own_len);
    for (index, node) in arena.iter().enumerate() {
        debug_assert_eq!(
            out.len(),
            offsets[index],
            "a node is emitted where it was laid out"
        );
        match node.terminal {
            Some(address) => {
                // The terminal is `flags` then `address`, and its size field says how many bytes
                // those are.
                uleb(
                    &mut out,
                    (uleb_len(EXPORT_SYMBOL_FLAGS_REGULAR) + uleb_len(address)) as u64,
                );
                uleb(&mut out, EXPORT_SYMBOL_FLAGS_REGULAR);
                uleb(&mut out, address);
            }
            None => uleb(&mut out, 0),
        }
        out.push(node.children.len() as u8);
        for (slot, &(byte, child)) in node.children.iter().enumerate() {
            out.push(byte);
            out.push(0);
            uleb_width(&mut out, offsets[child] as u64, widths[index][slot]);
        }
    }
    Ok(out)
}

/// The bytes a ULEB128 of `value` takes.
fn uleb_len(value: u64) -> usize {
    let mut shifts = 0;
    let mut rest = value;
    while rest >= 0x80 {
        rest >>= 7;
        shifts += 1;
    }
    shifts + 1
}

/// The widest ULEB128 this writes: enough for any offset an image of this size can hold.
const ULEB_MAX: usize = 5;

/// Appends `value` as a ULEB128.
fn uleb(out: &mut Vec<u8>, value: u64) {
    let mut rest = value;
    loop {
        let byte = (rest & 0x7f) as u8;
        rest >>= 7;
        if rest == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends `value` as a ULEB128 in exactly `width` bytes.
///
/// The layout above settles the width it laid a node out with, and this writes that many bytes
/// rather than the shortest encoding of the value: the two agree at the layout's fixpoint, and
/// writing any other length would move every node after it.
fn uleb_width(out: &mut Vec<u8>, value: u64, width: usize) {
    debug_assert!(
        uleb_len(value) <= width,
        "the layout reserved too few bytes"
    );
    let mut rest = value;
    for slot in 0..width {
        let byte = (rest & 0x7f) as u8;
        rest >>= 7;
        out.push(if slot + 1 == width { byte } else { byte | 0x80 });
    }
}

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
        while !self.out.len().is_multiple_of(8) {
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
    // A symbol in this format carries a leading underscore, and a loader looks a C name up with it
    // applied: `dlsym` asking for `x` searches for `_x`. The symbol table and the export trie
    // therefore both spell every name this way, which is also what `nm` and `dyld_info` print.
    let symbols: Vec<Vec<u8>> = names
        .iter()
        .map(|name| {
            let mut symbol = Vec::with_capacity(name.len() + 1);
            symbol.push(b'_');
            symbol.extend_from_slice(name.as_bytes());
            symbol
        })
        .collect();
    let mut strtab = vec![0];
    let mut name_offsets = Vec::with_capacity(symbols.len());
    for symbol in &symbols {
        name_offsets.push(push_cstr(&mut strtab, symbol)?);
    }

    let id_dylib_len = align(DYLIB_COMMAND_SIZE + INSTALL_NAME.len() + 1, 8);
    let segment_len = SEGMENT_COMMAND_SIZE + SECTION_SIZE;
    // Header, then `LC_ID_DYLIB`, `LC_SEGMENT_64` for `__TEXT`, `LC_SEGMENT_64` for `__LINKEDIT`,
    // `LC_UUID`, `LC_SYMTAB`, `LC_DYSYMTAB` and `LC_DYLD_INFO_ONLY`, in that order.
    // `LC_CODE_SIGNATURE` is deliberately absent: `codesign` appends the command with the
    // signature, while an image that carries the command and no signature is one `codesign` rejects
    // as malformed instead of signing.
    let commands_len = id_dylib_len
        + segment_len
        + SEGMENT_COMMAND_SIZE
        + UUID_COMMAND_SIZE
        + SYMTAB_COMMAND_SIZE
        + DYSYMTAB_COMMAND_SIZE
        + DYLD_INFO_COMMAND_SIZE;
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
    // The export trie follows the string table, and it ends the file. Neither is padded: a loader
    // reads both by offset, and the next thing on disk is the signature `codesign` appends, which
    // it aligns itself. All three are linkedit data, so `__LINKEDIT` covers the run from the first
    // of them to the end.
    let exports = export_trie(&symbols, text_off)?;
    let export_off = align(str_off + strtab.len(), LINKEDIT_ALIGN);
    let file_len = export_off + exports.len();
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

    let command_count = 7u32;
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

    // LC_DYLD_INFO_ONLY. The image has no rebases, binds or lazy binds: it references nothing
    // outside itself, and every address it carries is relative to its own sections. What it does
    // carry is the export trie, which is where a loader looks an exported name up.
    out.u32(LC_DYLD_INFO_ONLY);
    out.u32(DYLD_INFO_COMMAND_SIZE as u32);
    out.u32(0); // rebase_off
    out.u32(0); // rebase_size
    out.u32(0); // bind_off
    out.u32(0); // bind_size
    out.u32(0); // weak_bind_off
    out.u32(0); // weak_bind_size
    out.u32(0); // lazy_bind_off
    out.u32(0); // lazy_bind_size
    out.u32(export_off as u32);
    out.u32(exports.len() as u32);

    debug_assert!(
        out.out.len() <= text_off,
        "load commands fit before the slots"
    );
    out.out
        .extend(std::iter::repeat_n(0u8, text_off - out.out.len()));

    for _ in &name_offsets {
        out.out.extend_from_slice(&crate::arch::asmstub::INERT_SLOT);
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
    // The padding that puts the trie on a `__LINKEDIT` boundary is part of the segment.
    out.out
        .extend(std::iter::repeat_n(0u8, export_off - out.out.len()));
    out.out.extend_from_slice(&exports);
    debug_assert_eq!(out.out.len(), file_len, "the export trie ends the file");
    Ok((out.out, text_off))
}

// ===== reading an image back =====

/// One symbol of an image's table.
pub(crate) struct ImageSymbol {
    /// The name as the rest of mirvm spells it: this format's leading underscore is dropped, so a
    /// caller compares the same string it would use anywhere else.
    pub(crate) name: Box<str>,
    /// The address the symbol is defined at in the image, or zero for one the loader resolves.
    pub(crate) value: u64,
    /// Whether the loader has to resolve it rather than the image defining it.
    pub(crate) undefined: bool,
    /// Whether the image marks it private, which is how this format says the loader cannot reach
    /// it by name even though the image defines it.
    pub(crate) private_extern: bool,
    /// Whether the image defines it and offers it to other images.
    pub(crate) exported: bool,
    /// Whether the image marks it a weak definition, which is how this format says two images may
    /// define one name and the linker picks either.
    pub(crate) weak: bool,
}

/// `n_type`'s type field, the value meaning "defined nowhere in this image", and the bit marking a
/// symbol the image defines but does not export.
const N_TYPE: u8 = 0x0e;
const N_UNDF: u8 = 0x00;
const N_PEXT: u8 = 0x10;

/// The symbols of a Mach-O image, in the order its table lists them.
pub(crate) fn symbols(bytes: &[u8]) -> Result<Vec<ImageSymbol>, String> {
    let (commands, _) = header(bytes)?;
    let mut cursor = HEADER_SIZE;
    for _ in 0..commands {
        let (command, size) = load_command(bytes, cursor)?;
        if command == LC_SYMTAB {
            return symbol_table(bytes, symtab_at(bytes, cursor)?);
        }
        cursor = cursor
            .checked_add(size as usize)
            .ok_or_else(|| "the load commands overrun the image".to_string())?;
    }
    Err("the image carries no symbol table".to_string())
}

/// The names of the symbols the loader has to resolve, which is what an image's own undefined
/// range is: the external symbols its table defines nowhere.
pub(crate) fn undefined_symbols(bytes: &[u8]) -> Result<Vec<Box<str>>, String> {
    Ok(symbols(bytes)?
        .into_iter()
        .filter(|symbol| symbol.undefined)
        .map(|symbol| symbol.name)
        .collect())
}

/// The symbols the image defines but does not export, by name and address.
///
/// An image exports every external symbol it does not mark private, so this is the set a caller
/// cannot reach by name through the loader however the image is loaded.
pub(crate) fn hidden_symbols(bytes: &[u8]) -> Result<Vec<(Box<str>, u64)>, String> {
    Ok(symbols(bytes)?
        .into_iter()
        .filter(|symbol| !symbol.undefined && symbol.private_extern)
        .map(|symbol| (symbol.name, symbol.value))
        .collect())
}

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
    flags_at: usize,
}

impl Section {
    /// The kind of callable list this section is, when it is one a loader runs.
    ///
    /// The pointer form is the one an old linker wrote for both lists; a modern one writes the
    /// initializer list as 32-bit offsets instead, which is why the offsets type answers for
    /// initializers alone.
    pub(crate) fn callable_list(&self) -> Option<CallableList> {
        match self.section_type {
            S_MOD_INIT_FUNC_POINTERS | S_MOD_TERM_FUNC_POINTERS => Some(CallableList::Pointers),
            S_INIT_FUNC_OFFSETS => Some(CallableList::Offsets),
            _ => None,
        }
    }

    /// Stop the loader running this section as a list of callables, leaving the list and everything
    /// else about the image where they are.
    ///
    /// Clearing the type rather than the contents is what the two measured hazards force: a loader
    /// rebases these slots, so a zeroed one arrives as the slide and is called, and pointing the
    /// section at nothing would move everything laid out after it. Only the type is load-bearing for
    /// the loader, and this file's caller signs the image again afterwards.
    pub(crate) fn detach(&self, bytes: &mut [u8]) -> Result<(), String> {
        let short = || "a section header lies outside the image".to_string();
        let attributes = read_u32(bytes, self.flags_at).ok_or_else(short)? & !SECTION_TYPE_MASK;
        let field = bytes
            .get_mut(self.flags_at..self.flags_at + 4)
            .ok_or_else(short)?;
        field.copy_from_slice(&(S_REGULAR | attributes).to_le_bytes());
        Ok(())
    }
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

/// The `LC_SYMTAB` fields at `cursor`: `symoff`, `nsyms`, `stroff` and `strsize`.
fn symtab_at(bytes: &[u8], cursor: usize) -> Result<[usize; 4], String> {
    let short = || "the image ends inside a load command".to_string();
    let mut fields = [0_usize; 4];
    for (index, field) in fields.iter_mut().enumerate() {
        let at = cursor + 8 + index * 4;
        *field = read_u32(bytes, at).ok_or_else(short)? as usize;
    }
    Ok(fields)
}

/// Walks `[symoff, nsyms, stroff, strsize]` into the symbols they name.
fn symbol_table(bytes: &[u8], fields: [usize; 4]) -> Result<Vec<ImageSymbol>, String> {
    let [symoff, nsyms, stroff, strsize] = fields;
    let strings = bytes
        .get(stroff..stroff + strsize)
        .ok_or_else(|| "the string table lies outside the image".to_string())?;
    let mut out = Vec::with_capacity(nsyms);
    for index in 0..nsyms {
        let at = symoff + index * NLIST_SIZE;
        let entry = bytes
            .get(at..at + NLIST_SIZE)
            .ok_or_else(|| "the symbol table lies outside the image".to_string())?;
        // `nlist_64` is `n_strx`, `n_type`, `n_sect`, `n_desc`, `n_value`.
        let name = name_of(
            strings,
            read_u32(bytes, at)
                .ok_or_else(|| "the symbol table lies outside the image".to_string())?
                as usize,
        )?;
        out.push(ImageSymbol {
            name,
            value: read_u64(entry, 8).ok_or_else(|| "a symbol entry is truncated".to_string())?,
            undefined: entry[4] & N_TYPE == N_UNDF && entry[4] & N_EXT != 0,
            private_extern: entry[4] & N_PEXT != 0,
            exported: entry[4] & N_TYPE != N_UNDF
                && entry[4] & N_EXT != 0
                && entry[4] & N_PEXT == 0,
            weak: u16::from_le_bytes([entry[6], entry[7]]) & N_WEAK_DEF != 0,
        });
    }
    Ok(out)
}

/// The symbol name at `offset` in the string table, without this format's leading underscore.
fn name_of(strings: &[u8], offset: usize) -> Result<Box<str>, String> {
    let tail = strings
        .get(offset..)
        .ok_or_else(|| "a symbol name lies outside the string table".to_string())?;
    let end = tail
        .iter()
        .position(|&byte| byte == 0)
        .ok_or_else(|| "a symbol name is not terminated".to_string())?;
    let name =
        std::str::from_utf8(&tail[..end]).map_err(|_| "a symbol name is not UTF-8".to_string())?;
    Ok(Box::from(name.strip_prefix('_').unwrap_or(name)))
}

/// Whether `bytes` is a 64-bit Mach-O image at all, which is what a caller holding a member of an
/// archive asks before reading symbols out of it.
pub(crate) fn is_image(bytes: &[u8]) -> bool {
    read_u32(bytes, 0) == Some(MH_MAGIC_64)
}

/// The image's `ncmds` and `sizeofcmds`.
fn header(bytes: &[u8]) -> Result<(u32, u32), String> {
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
fn load_command(bytes: &[u8], cursor: usize) -> Result<(u32, u32), String> {
    let short = || "the image ends inside a load command".to_string();
    Ok((
        read_u32(bytes, cursor).ok_or_else(short)?,
        read_u32(bytes, cursor + 4).ok_or_else(short)?,
    ))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let slice = bytes.get(at..at + 8)?;
    Some(u64::from_le_bytes(slice.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 3] = ["mirvm_probe_a", "mirvm_probe_b", "mirvm_probe_c"];

    fn symbol_image() -> (Vec<u8>, usize) {
        let names: Vec<Box<str>> = NAMES.iter().map(|name| Box::from(*name)).collect();
        build(&names).expect("a symbol image")
    }

    /// The writer and the reader are the two halves of one layout: what the first lays down, the
    /// second has to find, at the addresses the first promised.
    #[test]
    fn the_reader_finds_what_the_writer_wrote() {
        let (bytes, text_off) = symbol_image();
        let symbols = symbols(&bytes).expect("an image this module wrote");
        assert_eq!(symbols.len(), NAMES.len());
        for (index, symbol) in symbols.iter().enumerate() {
            assert_eq!(&*symbol.name, NAMES[index]);
            assert_eq!(symbol.value as usize, text_off + index * SLOT);
            assert!(!symbol.undefined);
        }
        assert!(undefined_symbols(&bytes).expect("an image").is_empty());
    }

    /// An undefined symbol is one the loader resolves: `N_EXT` with no section. Turning the
    /// writer's first entry into one is what a caller of `undefined_symbols` is looking for.
    #[test]
    fn an_undefined_symbol_is_reported_without_the_formats_underscore() {
        let (mut bytes, _) = symbol_image();
        // Make the first entry external and sectionless, which is what "the loader resolves this"
        // is: the reader has to report it, and without the underscore the file spells it with.
        let symoff = first_nlist(&bytes);
        bytes[symoff + 4] = N_EXT;
        bytes[symoff + 5] = 0;
        let undefined = undefined_symbols(&bytes).expect("an image");
        assert_eq!(undefined.len(), 1);
        assert_eq!(&*undefined[0], NAMES[0]);
    }

    /// A symbol the image defines but does not export is the one a caller cannot reach by name
    /// through the loader, which is what the fallback table is for.
    #[test]
    fn a_private_symbol_is_reported_with_its_address() {
        let (mut bytes, text_off) = symbol_image();
        let symoff = first_nlist(&bytes);
        bytes[symoff + 4] |= N_PEXT;
        let hidden = hidden_symbols(&bytes).expect("an image");
        assert_eq!(hidden.len(), 1);
        assert_eq!(&*hidden[0].0, NAMES[0]);
        assert_eq!(hidden[0].1 as usize, text_off);
    }

    /// The file offset of the first `nlist_64`, for a test that rewrites one entry's `n_type`.
    fn first_nlist(bytes: &[u8]) -> usize {
        let mut at = HEADER_SIZE;
        loop {
            let (command, size) = load_command(bytes, at).expect("a load command");
            if command == LC_SYMTAB {
                return symtab_at(bytes, at).expect("symtab fields")[0];
            }
            at += size as usize;
        }
    }

    /// The image's export trie, found the way a loader finds it.
    fn trie_of(bytes: &[u8]) -> &[u8] {
        let (commands, _) = header(bytes).expect("a header");
        let mut at = HEADER_SIZE;
        for _ in 0..commands {
            let (command, size) = load_command(bytes, at).expect("a load command");
            if command == LC_DYLD_INFO_ONLY {
                // `dyld_info_command` is the command and its size, then ten 32-bit fields, of which
                // the export pair is the ninth and tenth.
                let off = read_u32(bytes, at + 8 + 8 * 4).expect("export_off") as usize;
                let len = read_u32(bytes, at + 8 + 9 * 4).expect("export_size") as usize;
                return &bytes[off..off + len];
            }
            at += size as usize;
        }
        panic!("the image carries no export trie");
    }

    fn read_uleb(bytes: &[u8], cursor: &mut usize) -> u64 {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = bytes[*cursor];
            *cursor += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }

    /// Walks the trie the way a loader does: a node's terminal is the name that ends there, and its
    /// children extend that name by their edge.
    fn walk(bytes: &[u8], at: usize, name: &mut String, out: &mut Vec<(String, u64)>) {
        let mut cursor = at;
        let terminal = read_uleb(bytes, &mut cursor) as usize;
        let table = cursor;
        if terminal != 0 {
            let _flags = read_uleb(bytes, &mut cursor);
            out.push((name.clone(), read_uleb(bytes, &mut cursor)));
        }
        cursor = table + terminal;
        let children = bytes[cursor] as usize;
        cursor += 1;
        for _ in 0..children {
            let end = cursor
                + bytes[cursor..]
                    .iter()
                    .position(|&b| b == 0)
                    .expect("a terminator");
            let edge = std::str::from_utf8(&bytes[cursor..end]).expect("an edge");
            cursor = end + 1;
            let offset = read_uleb(bytes, &mut cursor) as usize;
            let len = name.len();
            name.push_str(edge);
            walk(bytes, offset, name, out);
            name.truncate(len);
        }
    }

    /// The trie is what a loader walks to resolve a name, so the test walks it the same way and
    /// checks that every symbol comes back at the address the symbol table gives it.
    #[test]
    fn the_export_trie_carries_every_symbol_at_its_address() {
        let (bytes, text_off) = symbol_image();
        let mut entries = Vec::new();
        walk(trie_of(&bytes), 0, &mut String::new(), &mut entries);
        let mut expected: Vec<(String, u64)> = NAMES
            .iter()
            .enumerate()
            .map(|(index, name)| (format!("_{name}"), (text_off + index * SLOT) as u64))
            .collect();
        entries.sort();
        expected.sort();
        assert_eq!(entries, expected);
    }

    /// A name that is a prefix of another is where a trie is not a list: the shorter name's own
    /// terminal sits at the node the longer one passes through, and a loader has to reach both.
    #[test]
    fn a_name_that_is_a_prefix_of_another_is_still_reached() {
        let names: Vec<Box<str>> = ["probe", "probe_long"]
            .iter()
            .map(|name| Box::from(*name))
            .collect();
        let (bytes, text_off) = build(&names).expect("an image");
        let mut entries = Vec::new();
        walk(trie_of(&bytes), 0, &mut String::new(), &mut entries);
        entries.sort();
        assert_eq!(entries.len(), 2);
        assert_eq!(&*entries[0].0, "_probe");
        assert_eq!(entries[0].1 as usize, text_off);
        assert_eq!(&*entries[1].0, "_probe_long");
        assert_eq!(entries[1].1 as usize, text_off + SLOT);
    }

    /// The segment walker and the writer are two halves of one layout: the sections the walker
    /// reports have to be the ones the writer laid down, at the addresses it promised.
    #[test]
    fn the_segment_walker_finds_what_the_writer_wrote() {
        let (bytes, text_off) = symbol_image();
        let image = super::image(&bytes).expect("an image this module wrote");
        let text = image
            .segments
            .iter()
            .find(|segment| segment.sections.iter().any(|s| s.sectname == "__text"))
            .expect("a text segment");
        assert!(text.is_executable());
        assert_eq!(
            text.sections.len(),
            1,
            "the writer lays down exactly one section"
        );
        let section = &text.sections[0];
        assert_eq!(section.segname, "__TEXT");
        assert_eq!(section.addr as usize, text_off);
        assert_eq!(section.size as usize, SLOT * NAMES.len());
        assert_eq!(section.callable_list(), None);
        assert_eq!(image.routines_init, None);
    }
}
