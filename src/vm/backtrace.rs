//! Guest backtrace symbol carrier for Linux/x86_64.
//!
//! The standard Rust symbolizer discovers loaded ELF objects with
//! `dl_iterate_phdr` and reads their symbol tables itself. We therefore expose
//! one inert byte per FuncId as an ordinary ELF function symbol. Nothing ever
//! calls these bytes; their addresses are opaque instruction-pointer tokens.

use std::io::{Seek, Write};
use std::os::fd::FromRawFd;

use super::ir::Module;
use crate::native::elf;

#[derive(Debug)]
pub struct SymbolImage {
    _file: std::fs::File,
    handle: usize,
}

impl Drop for SymbolImage {
    fn drop(&mut self) {
        unsafe { crate::os::dll::close(self.handle) };
    }
}

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

fn build_elf(names: &[Box<str>]) -> Result<(Vec<u8>, usize), String> {
    const EHDR: usize = 64;
    const PHDR: usize = 56;
    const SHDR: usize = 64;
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

    elf::put_u16(&mut out, 16, 3); // ET_DYN
    elf::put_u16(&mut out, elf::ehdr::MACHINE, crate::arch::ELF_MACHINE);
    elf::put_u32(&mut out, 20, 1);
    elf::put_u64(&mut out, 32, EHDR as u64);
    elf::put_u64(&mut out, 40, shoff as u64);
    elf::put_u16(&mut out, 52, EHDR as u16);
    elf::put_u16(&mut out, 54, PHDR as u16);
    elf::put_u16(&mut out, 56, PHNUM as u16);
    elf::put_u16(&mut out, 58, SHDR as u16);
    elf::put_u16(&mut out, 60, SHNUM as u16);
    elf::put_u16(&mut out, 62, 8);

    // One read/execute load segment plus the dynamic table it contains.
    elf::put_u32(&mut out, EHDR, 1); // PT_LOAD
    elf::put_u32(&mut out, EHDR + 4, 5); // PF_R | PF_X
    elf::put_u64(&mut out, EHDR + 32, file_len as u64);
    elf::put_u64(&mut out, EHDR + 40, file_len as u64);
    elf::put_u64(&mut out, EHDR + 48, 0x1000);
    let dynamic_ph = EHDR + PHDR;
    elf::put_u32(&mut out, dynamic_ph, 2); // PT_DYNAMIC
    elf::put_u32(&mut out, dynamic_ph + 4, 4); // PF_R
    elf::put_u64(&mut out, dynamic_ph + 8, dynamic_off as u64);
    elf::put_u64(&mut out, dynamic_ph + 16, dynamic_off as u64);
    elf::put_u64(&mut out, dynamic_ph + 24, dynamic_off as u64);
    elf::put_u64(&mut out, dynamic_ph + 32, dynamic_len as u64);
    elf::put_u64(&mut out, dynamic_ph + 40, dynamic_len as u64);
    elf::put_u64(&mut out, dynamic_ph + 48, 8);

    for index in 0..names.len() {
        out[text_off + index * SLOT] = crate::arch::x86_64::asmstub::RET; // never executed
        out[text_off + index * SLOT + 1..text_off + (index + 1) * SLOT].fill(0x90);
    }
    out[dynstr_off..dynstr_off + dynstr.len()].copy_from_slice(&dynstr);
    out[strtab_off..strtab_off + strtab.len()].copy_from_slice(&strtab);
    out[shstr_off..shstr_off + shstr.len()].copy_from_slice(&shstr);

    for (index, &name) in name_offsets.iter().enumerate() {
        let write_symbol = |out: &mut [u8], base: usize| {
            elf::put_u32(out, base, name);
            out[base + 4] = 0x12; // STB_GLOBAL | STT_FUNC
            elf::put_u16(out, base + 6, 1); // .text
            elf::put_u64(out, base + 8, (text_off + index * SLOT) as u64);
            elf::put_u64(out, base + 16, SLOT as u64);
        };
        write_symbol(&mut out, dynsym_off + (index + 1) * 24);
        write_symbol(&mut out, symtab_off + (index + 1) * 24);
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
        (4u64, hash_off as u64),
        (5, dynstr_off as u64),
        (6, dynsym_off as u64),
        (10, dynstr.len() as u64),
        (11, 24),
        (0, 0),
    ]
    .into_iter()
    .enumerate()
    {
        elf::put_u64(&mut out, dynamic_off + index * 16, tag);
        elf::put_u64(&mut out, dynamic_off + index * 16 + 8, value);
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
    section(1, sh_name[0], 1, 6, text_off, text_len, 0, 0, 16, 0);
    section(2, sh_name[1], 3, 2, dynstr_off, dynstr.len(), 0, 0, 1, 0);
    section(3, sh_name[2], 11, 2, dynsym_off, sym_len, 2, 1, 8, 24);
    section(4, sh_name[3], 5, 2, hash_off, hash_len, 3, 0, 4, 4);
    section(5, sh_name[4], 6, 2, dynamic_off, dynamic_len, 2, 0, 8, 16);
    section(6, sh_name[5], 3, 0, strtab_off, strtab.len(), 0, 0, 1, 0);
    section(7, sh_name[6], 2, 0, symtab_off, sym_len, 6, 1, 8, 24);
    section(8, sh_name[7], 3, 0, shstr_off, shstr.len(), 0, 0, 1, 0);
    Ok((out, text_off))
}

pub fn materialize_symbols(module: &mut Module) -> Result<(), String> {
    if module.function_names.is_empty() || module.backtrace_image.is_some() {
        return Ok(());
    }
    let (bytes, text_off) = build_elf(&module.function_names)?;
    let Some(fd) = crate::os::mem::anonymous_file(c"mirvm-guest-symbols") else {
        return Err(format!(
            "memfd_create failed: {}",
            std::io::Error::last_os_error()
        ));
    };
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(&bytes)
        .map_err(|error| format!("writing in-memory ELF failed: {error}"))?;
    file.rewind()
        .map_err(|error| format!("rewinding in-memory ELF failed: {error}"))?;
    let path = std::ffi::CString::new(format!("/proc/self/fd/{fd}"))
        .map_err(|_| "in-memory ELF fd path unexpectedly contains NUL".to_string())?;
    let handle = crate::os::dll::open_with_flags(
        &path,
        crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
    )
    .map_err(|error| format!("dlopen of in-memory ELF failed: {error}"))?;
    let bias = crate::os::dll::load_bias(handle).ok_or("reading in-memory ELF load base failed")?;
    module.backtrace_ips = (0..module.function_names.len())
        .map(|index| (bias + text_off + index * 16) as u64)
        .collect();
    module.backtrace_image = Some(SymbolImage {
        _file: file,
        handle,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_elf_exports_function_names_at_indexed_addresses() {
        let mut module = Module {
            function_names: vec![
                "mirvm_backtrace_probe_a".into(),
                "mirvm_backtrace_probe_b".into(),
            ],
            ..Module::default()
        };
        materialize_symbols(&mut module).unwrap();
        assert_eq!(module.backtrace_ips.len(), 2);
        for (index, expected) in ["mirvm_backtrace_probe_a", "mirvm_backtrace_probe_b"]
            .iter()
            .enumerate()
        {
            let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
            let ok = unsafe {
                libc::dladdr(
                    module.backtrace_ips[index] as *const libc::c_void,
                    info.as_mut_ptr(),
                )
            };
            assert_ne!(ok, 0);
            let info = unsafe { info.assume_init() };
            assert!(!info.dli_sname.is_null());
            assert_eq!(
                unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }
                    .to_str()
                    .unwrap(),
                *expected
            );
            let symbol = std::ffi::CString::new(*expected).unwrap();
            assert_eq!(
                crate::os::dll::sym(module.backtrace_image.as_ref().unwrap().handle, &symbol,),
                module.backtrace_ips[index] as usize
            );
        }
    }
}
