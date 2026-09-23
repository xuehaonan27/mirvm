//! An ELF object whose only content is a symbol per guest function.
//!
//! A process symbolizer discovers the objects a process has loaded and reads their symbol tables
//! itself, so the way to make a synthetic instruction-pointer token resolvable is to hand it a
//! real, loadable ELF that defines a symbol at that address. The image is one inert byte per
//! function -- a return instruction followed by padding, `SLOT` bytes apart, so an address is
//! always a symbol start and never mid-instruction -- plus the section and dynamic tables a
//! loader expects. Nothing ever executes these bytes; their addresses are opaque tokens.
//!
//! [`build`] returns the bytes and the file offset the functions start at, which is what a caller
//! adds to the load bias to get each token.

use crate::arch::x86_64::asmstub::RET;
use crate::native::elf;

fn align(value: usize, alignment: usize) -> usize {
    value.div_ceil(alignment) * alignment
}

fn push_cstr(table: &mut Vec<u8>, value: &[u8]) -> Result<u32, String> {
    if value.contains(&0) {
        return Err("guest function name contains NUL".into());
    }
    let offset = u32::try_from(table.len()).map_err(|_| "ELF string table too large")?;
    table.extend_from_slice(value);
    table.push(0);
    Ok(offset)
}

pub fn build(names: &[Box<str>]) -> Result<(Vec<u8>, usize), String> {
    /// The section index of `.text`, which every synthetic symbol is defined in.
    const TEXT_SECTION_INDEX: u16 = 1;
    const EHDR: usize = elf::EHDR_SIZE;
    const PHDR: usize = elf::PHDR_SIZE;
    const SHDR: usize = elf::SHDR_SIZE;
    /// The section index of the section-header string table.
    const SHSTRTAB_INDEX: usize = 8;
    const PHNUM: usize = 2;
    const SHNUM: usize = 9;
    const SLOT: usize = 16;

    let mut dynstr = vec![0];
    let mut name_offsets = Vec::with_capacity(names.len());
    for name in names {
        name_offsets.push(push_cstr(&mut dynstr, name.as_bytes())?);
    }
    let strtab = dynstr.clone();
    let mut shstr = vec![0];
    let section_names = [
        ".text",
        ".dynstr",
        ".dynsym",
        ".hash",
        ".dynamic",
        ".strtab",
        ".symtab",
        ".shstrtab",
    ];
    let mut sh_name = Vec::new();
    for name in section_names {
        sh_name.push(push_cstr(&mut shstr, name.as_bytes())?);
    }

    let text_off = align(EHDR + PHNUM * PHDR, SLOT);
    let text_len = names.len().checked_mul(SLOT).ok_or("ELF text too large")?;
    let dynstr_off = text_off + text_len;
    let dynsym_off = align(dynstr_off + dynstr.len(), 8);
    let sym_len = (names.len() + 1)
        .checked_mul(24)
        .ok_or("ELF symtab too large")?;
    let hash_off = align(dynsym_off + sym_len, 4);
    let hash_len = (2 + 1 + names.len() + 1)
        .checked_mul(4)
        .ok_or("ELF hash too large")?;
    let dynamic_off = align(hash_off + hash_len, 8);
    let dynamic_len = 6 * 16;
    let strtab_off = dynamic_off + dynamic_len;
    let symtab_off = align(strtab_off + strtab.len(), 8);
    let shstr_off = symtab_off + sym_len;
    let shoff = align(shstr_off + shstr.len(), 8);
    let file_len = shoff + SHNUM * SHDR;
    let mut out = vec![0u8; file_len];

    out[..elf::IDENT.len()].copy_from_slice(&elf::IDENT);
    out[4] = elf::ELFCLASS64;
    out[5] = elf::ELFDATA2LSB;
    out[6] = 1; // EI_VERSION, the only value this format defines

    elf::put_u16(&mut out, elf::ehdr::TYPE, elf::ET_DYN);
    elf::put_u16(&mut out, elf::ehdr::MACHINE, crate::arch::ELF_MACHINE);
    elf::put_u32(&mut out, elf::ehdr::VERSION, 1);
    elf::put_u64(&mut out, elf::ehdr::PHOFF, EHDR as u64);
    elf::put_u64(&mut out, elf::ehdr::SHOFF, shoff as u64);
    elf::put_u16(&mut out, elf::ehdr::EHSIZE, EHDR as u16);
    elf::put_u16(&mut out, elf::ehdr::PHENTSIZE, PHDR as u16);
    elf::put_u16(&mut out, elf::ehdr::PHNUM, PHNUM as u16);
    elf::put_u16(&mut out, elf::ehdr::SHENTSIZE, SHDR as u16);
    elf::put_u16(&mut out, elf::ehdr::SHNUM, SHNUM as u16);
    elf::put_u16(&mut out, elf::ehdr::SHSTRNDX, SHSTRTAB_INDEX as u16);

    // One read/execute load segment plus the dynamic table it contains.
    elf::put_u32(&mut out, EHDR, elf::PT_LOAD);
    elf::put_u32(&mut out, EHDR + 4, elf::PF_R | elf::PF_X);
    elf::put_u64(&mut out, EHDR + 32, file_len as u64);
    elf::put_u64(&mut out, EHDR + 40, file_len as u64);
    elf::put_u64(&mut out, EHDR + 48, 0x1000);
    let dynamic_ph = EHDR + PHDR;
    elf::put_u32(&mut out, dynamic_ph, elf::PT_DYNAMIC);
    elf::put_u32(&mut out, dynamic_ph + 4, elf::PF_R);
    elf::put_u64(&mut out, dynamic_ph + 8, dynamic_off as u64);
    elf::put_u64(&mut out, dynamic_ph + 16, dynamic_off as u64);
    elf::put_u64(&mut out, dynamic_ph + 24, dynamic_off as u64);
    elf::put_u64(&mut out, dynamic_ph + 32, dynamic_len as u64);
    elf::put_u64(&mut out, dynamic_ph + 40, dynamic_len as u64);
    elf::put_u64(&mut out, dynamic_ph + 48, 8);

    for index in 0..names.len() {
        out[text_off + index * SLOT] = RET; // never executed
        out[text_off + index * SLOT + 1..text_off + (index + 1) * SLOT].fill(0x90);
    }
    out[dynstr_off..dynstr_off + dynstr.len()].copy_from_slice(&dynstr);
    out[strtab_off..strtab_off + strtab.len()].copy_from_slice(&strtab);
    out[shstr_off..shstr_off + shstr.len()].copy_from_slice(&shstr);

    for (index, &name) in name_offsets.iter().enumerate() {
        let write_symbol = |out: &mut [u8], base: usize| {
            elf::put_u32(out, base, name);
            out[base + 4] = 0x12; // STB_GLOBAL | STT_FUNC
            elf::put_u16(out, base + 6, TEXT_SECTION_INDEX);
            elf::put_u64(out, base + 8, (text_off + index * SLOT) as u64);
            elf::put_u64(out, base + 16, SLOT as u64);
        };
        write_symbol(&mut out, dynsym_off + (index + 1) * elf::SYM_ENTRY_SIZE);
        write_symbol(&mut out, symtab_off + (index + 1) * elf::SYM_ENTRY_SIZE);
    }

    // SysV hash: one bucket, every symbol in a single chain.
    elf::put_u32(&mut out, hash_off, 1);
    elf::put_u32(&mut out, hash_off + 4, (names.len() + 1) as u32);
    elf::put_u32(&mut out, hash_off + 8, u32::from(!names.is_empty()));
    for index in 1..=names.len() {
        let next = if index == names.len() {
            0
        } else {
            (index + 1) as u32
        };
        elf::put_u32(&mut out, hash_off + 12 + index * 4, next);
    }

    for (index, (tag, value)) in [
        (elf::DT_HASH as u64, hash_off as u64),
        (elf::DT_STRTAB as u64, dynstr_off as u64),
        (elf::DT_SYMTAB as u64, dynsym_off as u64),
        (elf::DT_STRSZ as u64, dynstr.len() as u64),
        (elf::DT_SYMENT as u64, elf::SYM_ENTRY_SIZE as u64),
        (elf::DT_NULL as u64, 0),
    ]
    .into_iter()
    .enumerate()
    {
        elf::put_u64(&mut out, dynamic_off + index * elf::DYN_ENTRY_SIZE, tag);
        elf::put_u64(
            &mut out,
            dynamic_off + index * elf::DYN_ENTRY_SIZE + 8,
            value,
        );
    }

    let mut section = |index: usize,
                       name: u32,
                       kind: u32,
                       flags: u64,
                       offset: usize,
                       len: usize,
                       link: u32,
                       info: u32,
                       alignment: u64,
                       entsize: u64| {
        let base = shoff + index * SHDR;
        elf::put_u32(&mut out, base, name);
        elf::put_u32(&mut out, base + 4, kind);
        elf::put_u64(&mut out, base + 8, flags);
        elf::put_u64(&mut out, base + 16, offset as u64);
        elf::put_u64(&mut out, base + 24, offset as u64);
        elf::put_u64(&mut out, base + 32, len as u64);
        elf::put_u32(&mut out, base + 40, link);
        elf::put_u32(&mut out, base + 44, info);
        elf::put_u64(&mut out, base + 48, alignment);
        elf::put_u64(&mut out, base + 56, entsize);
    };
    section(
        1,
        sh_name[0],
        elf::SHT_PROGBITS,
        6,
        text_off,
        text_len,
        0,
        0,
        16,
        0,
    );
    section(
        2,
        sh_name[1],
        elf::SHT_STRTAB,
        2,
        dynstr_off,
        dynstr.len(),
        0,
        0,
        1,
        0,
    );
    section(
        3,
        sh_name[2],
        elf::SHT_DYNSYM,
        2,
        dynsym_off,
        sym_len,
        2,
        1,
        8,
        elf::SYM_ENTRY_SIZE as u64,
    );
    section(
        4,
        sh_name[3],
        elf::SHT_HASH,
        2,
        hash_off,
        hash_len,
        3,
        0,
        4,
        4,
    );
    section(
        5,
        sh_name[4],
        elf::SHT_DYNAMIC,
        2,
        dynamic_off,
        dynamic_len,
        2,
        0,
        8,
        elf::DYN_ENTRY_SIZE as u64,
    );
    section(
        6,
        sh_name[5],
        elf::SHT_STRTAB,
        0,
        strtab_off,
        strtab.len(),
        0,
        0,
        1,
        0,
    );
    section(
        7,
        sh_name[6],
        elf::SHT_SYMTAB,
        0,
        symtab_off,
        sym_len,
        6,
        1,
        8,
        elf::SYM_ENTRY_SIZE as u64,
    );
    section(
        8,
        sh_name[7],
        elf::SHT_STRTAB,
        0,
        shstr_off,
        shstr.len(),
        0,
        0,
        1,
        0,
    );
    Ok((out, text_off))
}
