//! `.symtab` fallback symbol resolution for static-archive `.so` files.
//!
//! When an archive built with `-fvisibility=hidden` (ring's build.rs passes that flag, as do
//! the zstd-sys family) is converted with `-shared --whole-archive`, its symbols are localized
//! and **do not enter .dynsym**: dlsym finds nothing, globally or by handle, but `.symtab`
//! keeps them intact (including their localized addresses).
//!
//! Resolution order (the same in all three call sites: FfiState direct calls, fn-ptr address
//! taking, and extern statics): **hidden symbols (this module's fallback table) come before
//! dlsym(RTLD_DEFAULT)**. This is a direct translation of native link-time binding semantics:
//! once a static archive member is linked into the guest binary, guest references always bind
//! to the definition inside the archive, overriding the global namespace. The host process
//! loads libLLVM through librustc_driver, which embeds and exports the whole `ZSTD_*` set
//! (`@@LLVM_22.1` versioned symbols), so letting dlsym win would silently bind the guest's
//! zstd calls to the host library (corpus c_zstd_stream: valid but different compressed
//! bytes). dynsym-visible symbols do not enter the fallback table; they keep their global
//! dlsym resolution (`reject_symbol_ambiguity` already rejects global collisions at
//! materialization time, and dlsym semantics such as IFUNC only hold on the dynamic surface).
//!
//! Only used for archives mirvm materializes itself (required_native_libs): their format comes
//! from native_archive's constrained link (ELF64 LE x86_64, not stripped). System libraries
//! always have a normal .dynsym and never take this path.
//!
//! The dlopen handle -> load base mapping (dlinfo) lives in `os::dll::load_bias`.

use std::collections::HashMap;

const SHT_DYNSYM: u32 = 11;
const SHT_SYMTAB: u32 = 2;

/// Resolve a `.so`'s `.symtab`: defined symbol name -> st_value (file virtual address,
/// relative to the load base). Err means it is not the expected ELF64 LE or the structure is
/// out of bounds (corrupted format, which a materialized artifact should never be). Production
/// code only uses hidden_symtab_values; this raw view exists for unit-test comparison (visible
/// symbols must have the same address on both paths).
#[cfg(test)]
pub(crate) fn symtab_values(so_path: &str) -> Result<HashMap<Box<str>, u64>, String> {
    symbol_table_values(so_path, SHT_SYMTAB)
}

/// The hidden-symbol fallback table = `.symtab` defined minus `.dynsym` defined.
/// dynsym-visible symbols keep their global dlsym resolution (`reject_symbol_ambiguity`
/// already rejects their collision with RTLD_DEFAULT at materialization time); only hidden
/// symbols, unreachable by dlsym, take the "archive before global" link-time binding semantics
/// (see the module header). A parse failure is propagated as Err and every caller degrades to
/// no table.
pub fn hidden_symtab_values(so_path: &str) -> Result<HashMap<Box<str>, u64>, String> {
    let mut syms = symbol_table_values(so_path, SHT_SYMTAB)?;
    for name in symbol_table_values(so_path, SHT_DYNSYM)?.keys() {
        syms.remove(&**name);
    }
    Ok(syms)
}

/// Resolve the given symbol table section (SHT_SYMTAB / SHT_DYNSYM; entries have the same
/// format): defined symbol name -> st_value (file virtual address, relative to the load base).
fn symbol_table_values(so_path: &str, want_sht: u32) -> Result<HashMap<Box<str>, u64>, String> {
    let bytes = std::fs::read(so_path)
        .map_err(|e| format!("failed to read archive shared library `{so_path}`: {e}"))?;
    let u16_at = |off: usize| -> Option<u16> {
        Some(u16::from_le_bytes(
            bytes.get(off..off + 2)?.try_into().ok()?,
        ))
    };
    let u32_at = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(off..off + 4)?.try_into().ok()?,
        ))
    };
    let u64_at = |off: usize| -> Option<u64> {
        Some(u64::from_le_bytes(
            bytes.get(off..off + 8)?.try_into().ok()?,
        ))
    };
    let bad = || {
        format!("archive shared library `{so_path}` is not the expected ELF64 LE (or is corrupted)")
    };
    if bytes.len() < 64 || bytes[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(bad());
    }
    if bytes[4] != 2 || bytes[5] != 1 {
        // EI_CLASS=ELFCLASS64, EI_DATA=ELFDATA2LSB
        return Err(bad());
    }
    let shoff = u64_at(0x28).ok_or_else(bad)? as usize;
    let shentsize = u16_at(0x3a).ok_or_else(bad)? as usize;
    let mut shnum = u16_at(0x3c).ok_or_else(bad)? as usize;
    if shentsize < 64 {
        return Err(bad());
    }
    let shdr = |i: usize| -> Option<(u32, u64, u64, u32, u64)> {
        // (sh_type, sh_offset, sh_size, sh_link, sh_entsize)
        let base = shoff.checked_add(i.checked_mul(shentsize)?)?;
        Some((
            u32_at(base + 4)?,
            u64_at(base + 24)?,
            u64_at(base + 32)?,
            u32_at(base + 40)?,
            u64_at(base + 56)?,
        ))
    };
    if shnum == 0 {
        // SHN_UNDEF extension: the real section count is in shdr[0].sh_size
        let (_, _, size, _, _) = shdr(0).ok_or_else(bad)?;
        shnum = usize::try_from(size).map_err(|_| bad())?;
    }
    for i in 0..shnum {
        let (ty, sym_off, sym_size, link, entsize) = shdr(i).ok_or_else(bad)?;
        if ty != want_sht {
            continue;
        }
        if entsize < 24 {
            return Err(bad());
        }
        let str_idx = usize::try_from(link).map_err(|_| bad())?;
        let (_, str_off, str_size, _, _) = shdr(str_idx).ok_or_else(bad)?;
        let str_end = usize::try_from(str_off + str_size).map_err(|_| bad())?;
        let mut out = HashMap::new();
        let count = usize::try_from(sym_size / entsize.max(1)).map_err(|_| bad())?;
        for j in 0..count {
            let base = usize::try_from(sym_off)
                .ok()
                .and_then(|o| o.checked_add(j.checked_mul(entsize as usize)?))
                .ok_or_else(bad)?;
            let st_name = u32_at(base).ok_or_else(bad)? as usize;
            let st_shndx = u16_at(base + 6).ok_or_else(bad)?;
            let st_value = u64_at(base + 8).ok_or_else(bad)?;
            // Skip SHN_UNDEF (0) and reserved section indices (0xff00+)
            if st_name == 0 || st_shndx == 0 || st_shndx >= 0xff00 {
                continue;
            }
            let name_start = usize::try_from(str_off)
                .ok()
                .and_then(|o| o.checked_add(st_name))
                .ok_or_else(bad)?;
            let name_end = bytes[name_start..str_end.min(bytes.len())]
                .iter()
                .position(|&b| b == 0)
                .map(|p| name_start + p)
                .ok_or_else(bad)?;
            let name = std::str::from_utf8(&bytes[name_start..name_end]).map_err(|_| bad())?;
            out.insert(Box::from(name), st_value);
        }
        return Ok(out);
    }
    // No requested symbol table section (should not happen for a materialized artifact,
    // stripped or not, but harmless): treat as empty
    Ok(HashMap::new())
}

// ===== ar archive SHN_UNDEF static enumeration (the "symbol is in the rlib" criterion for
// native-archive closures) =====

/// Static enumeration of a Unix ar archive's undefined symbols: walk member by member
/// (skipping the ar symbol table and long-name table members), read each ELF64 member's
/// `.symtab`, and collect the names of GLOBAL/WEAK symbols with `SHN_UNDEF`. This parses bytes
/// (the same structural walk as `symbol_table_values`) and never parses tool text output.
pub fn archive_undefined_symbols(archive_path: &str) -> Result<Vec<Box<str>>, String> {
    let bytes = std::fs::read(archive_path)
        .map_err(|e| format!("failed to read static native archive `{archive_path}`: {e}"))?;
    archive_undefined_symbols_in(&bytes).map_err(|why| {
        format!(
            "undefined-symbol enumeration failed for static native archive `{archive_path}`: {why}"
        )
    })
}

/// Byte-level implementation (the member header chain is a fixed 60B record: name[16]
/// date[12] uid[6] gid[6] mode[8] size[10] "`\n"; member bodies are aligned to 2 after size).
fn archive_undefined_symbols_in(bytes: &[u8]) -> Result<Vec<Box<str>>, String> {
    if !bytes.starts_with(b"!<arch>\n") {
        return Err("not a Unix ar archive".into());
    }
    let mut out: Vec<Box<str>> = Vec::new();
    let mut pos = 8usize;
    while pos + 60 <= bytes.len() {
        let hdr = &bytes[pos..pos + 60];
        if &hdr[58..60] != b"`\n" {
            return Err(format!("ar member header magic misplaced @{pos:#x}"));
        }
        let size_txt =
            std::str::from_utf8(&hdr[48..58]).map_err(|_| "ar member size is not ASCII")?;
        let size: usize = size_txt
            .trim()
            .parse()
            .map_err(|_| format!("ar member size is unparsable `{size_txt}`"))?;
        let body_end = pos + 60 + size;
        if body_end > bytes.len() {
            return Err("ar member body out of bounds".into());
        }
        // Classifying ar member metadata must be exact: GNU ar references names longer than 15
        // characters through the string table as `/N` (e.g. `/0`), so a leading '/' does
        // **not** imply metadata. The only metadata members are the symbol table (`/`,
        // `__.SYMDEF`, `/SYM64/`) and the string table (`//`).
        let name = String::from_utf8_lossy(&hdr[0..16]);
        let name = name.trim();
        let is_metadata = name == "/"
            || name == "//"
            || name == "__.SYMDEF"
            || name == "__.SYMDEF SORTED"
            || name == "/SYM64/";
        if !is_metadata {
            let mut body = &bytes[pos + 60..body_end];
            // BSD-style `/#1/<len>`: the name is embedded at the start of the body, so strip
            // its length before reading the content
            if let Some(rest) = name.strip_prefix("/#1/")
                && let Ok(nlen) = rest.trim().parse::<usize>()
            {
                body = &body[nlen.min(body.len())..];
            }
            if body.starts_with(b"\x7fELF") {
                for sym in elf_undefined_symbols(body)? {
                    if !out.contains(&sym) {
                        out.push(sym);
                    }
                }
            }
            // Non-ELF member (text listing, etc.): skip
        }
        pos = body_end + (size & 1);
    }
    Ok(out)
}

/// SHN_UNDEF enumeration over a single ELF64 LE byte slice (GLOBAL/WEAK bindings; the same
/// structural walk as symbol_table_values, but selecting shndx == 0 with no filtering).
fn elf_undefined_symbols(bytes: &[u8]) -> Result<Vec<Box<str>>, String> {
    let u16_at = |off: usize| -> Option<u16> {
        Some(u16::from_le_bytes(
            bytes.get(off..off + 2)?.try_into().ok()?,
        ))
    };
    let u32_at = |off: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(off..off + 4)?.try_into().ok()?,
        ))
    };
    let u64_at = |off: usize| -> Option<u64> {
        Some(u64::from_le_bytes(
            bytes.get(off..off + 8)?.try_into().ok()?,
        ))
    };
    let bad = || "not the expected ELF64 LE (or corrupted)".to_string();
    if bytes.len() < 64 || bytes[0..4] != [0x7f, b'E', b'L', b'F'] || bytes[4] != 2 || bytes[5] != 1
    {
        return Err(bad());
    }
    let shoff = u64_at(0x28).ok_or_else(bad)? as usize;
    let shentsize = u16_at(0x3a).ok_or_else(bad)? as usize;
    let mut shnum = u16_at(0x3c).ok_or_else(bad)? as usize;
    if shentsize < 64 {
        return Err(bad());
    }
    let shdr = |i: usize| -> Option<(u32, u64, u64, u32, u64)> {
        let base = shoff.checked_add(i.checked_mul(shentsize)?)?;
        Some((
            u32_at(base + 4)?,
            u64_at(base + 24)?,
            u64_at(base + 32)?,
            u32_at(base + 40)?,
            u64_at(base + 56)?,
        ))
    };
    if shnum == 0 {
        let (_, _, size, _, _) = shdr(0).ok_or_else(bad)?;
        shnum = usize::try_from(size).map_err(|_| bad())?;
    }
    for i in 0..shnum {
        let (ty, sym_off, sym_size, link, entsize) = shdr(i).ok_or_else(bad)?;
        if ty != SHT_SYMTAB {
            continue;
        }
        if entsize < 24 {
            return Err(bad());
        }
        let str_idx = usize::try_from(link).map_err(|_| bad())?;
        let (_, str_off, str_size, _, _) = shdr(str_idx).ok_or_else(bad)?;
        let str_end = usize::try_from(str_off + str_size).map_err(|_| bad())?;
        let count = usize::try_from(sym_size / entsize.max(1)).map_err(|_| bad())?;
        let mut out = Vec::new();
        for j in 0..count {
            let base = usize::try_from(sym_off)
                .ok()
                .and_then(|o| o.checked_add(j.checked_mul(entsize as usize)?))
                .ok_or_else(bad)?;
            let st_name = u32_at(base).ok_or_else(bad)? as usize;
            let st_info = bytes.get(base + 4).copied().ok_or_else(bad)?;
            let st_shndx = u16_at(base + 6).ok_or_else(bad)?;
            let bind = st_info >> 4;
            // Take only undefined (SHN_UNDEF) global/weak bindings (LOCAL is the member's
            // internal business)
            if st_name == 0 || st_shndx != 0 || (bind != 1 && bind != 2) {
                continue;
            }
            let name_start = usize::try_from(str_off)
                .ok()
                .and_then(|o| o.checked_add(st_name))
                .ok_or_else(bad)?;
            let name_end = bytes[name_start..str_end.min(bytes.len())]
                .iter()
                .position(|&b| b == 0)
                .map(|p| name_start + p)
                .ok_or_else(bad)?;
            let name = std::str::from_utf8(&bytes[name_start..name_end]).map_err(|_| bad())?;
            out.push(Box::from(name));
        }
        return Ok(out);
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::{archive_undefined_symbols, hidden_symtab_values, symtab_values};
    use std::ffi::CString;
    use std::process::Command;

    /// Static SHN_UNDEF enumeration of an ar archive: with definitions and undefined symbols
    /// mixed together, plus LOCAL and ar metadata members, only GLOBAL/WEAK undefined symbols
    /// are reported.
    #[test]
    fn archive_undefined_symbols_reports_global_and_weak_undef_only() {
        let dir = std::env::temp_dir().join(format!("mirvm-elfsym-arundef-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (c1, o1, c2, o2, a) = (
            dir.join("a.c"),
            // A member name longer than 15 characters forces GNU ar into the string-table
            // reference (/N) form
            dir.join("very-long-member-name-a.o"),
            dir.join("b.c"),
            dir.join("b.o"),
            dir.join("libp.a"),
        );
        // a.o: an undefined reference to an rlib-side symbol (GLOBAL) + a weak undefined + its
        // own definitions (one global, one static). A "used but never defined" static is
        // emitted as a GLOBAL undef by this cc as well, so it does not exercise the LOCAL
        // filter; the static definition does.
        std::fs::write(
            &c1,
            "extern int rlib_side_def(long);\n\
             __attribute__((weak)) extern int weak_missing(void);\n\
             int defined_here(void) { return 1; }\n\
             static int static_defined(void) { return 2; }\n\
             int tramp(long x) { return rlib_side_def(x) + weak_missing() + defined_here() + static_defined(); }\n",
        )
        .unwrap();
        // b.o: all definitions, no undefined symbols
        std::fs::write(&c2, "int other(void) { return 2; }\n").unwrap();
        for (c, o) in [(&c1, &o1), (&c2, &o2)] {
            assert!(
                Command::new("cc")
                    .args(["-fPIC", "-c"])
                    .arg(c)
                    .arg("-o")
                    .arg(o)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert!(
            Command::new("ar")
                .args(["crs"])
                .arg(&a)
                .arg(&o1)
                .arg(&o2)
                .status()
                .unwrap()
                .success()
        );
        let undef = archive_undefined_symbols(a.to_str().unwrap()).unwrap();
        assert!(
            undef.iter().any(|s| &**s == "rlib_side_def"),
            "GLOBAL undef missed: {undef:?}"
        );
        assert!(
            undef.iter().any(|s| &**s == "weak_missing"),
            "WEAK undef missed: {undef:?}"
        );
        assert!(
            !undef.iter().any(|s| &**s == "defined_here"),
            "defined symbol falsely reported: {undef:?}"
        );
        assert!(
            !undef.iter().any(|s| &**s == "static_defined"),
            "defined static symbol falsely reported: {undef:?}"
        );
        assert!(
            !undef.iter().any(|s| &**s == "other"),
            "defined symbol from the second member falsely reported: {undef:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Link an archive with -fvisibility=hidden using native_archive's parameters: its symbols
    /// do not enter .dynsym, but the .symtab fallback must resolve the same address that a
    /// direct dlsym returns.
    #[test]
    fn hidden_symbols_resolve_via_symtab_with_same_address_as_dlsym() {
        let dir = std::env::temp_dir().join(format!("mirvm-elfsym-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (c, o, a, so) = (
            dir.join("p.c"),
            dir.join("p.o"),
            dir.join("libp.a"),
            dir.join("libp.so"),
        );
        std::fs::write(
            &c,
            "__attribute__((visibility(\"hidden\"))) unsigned long mirvm_hidden_probe(void) { return 0x2aUL; }\n\
             unsigned long mirvm_visible_probe(void) { return mirvm_hidden_probe(); }\n",
        )
        .unwrap();
        assert!(
            Command::new("cc")
                .args(["-fPIC", "-c",])
                .arg(&c)
                .arg("-o")
                .arg(&o)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("ar")
                .args(["crs"])
                .arg(&a)
                .arg(&o)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("cc")
                .args(["-shared", "-Wl,-z,defs", "-Wl,--whole-archive"])
                .arg(&a)
                .args(["-Wl,--no-whole-archive", "-o"])
                .arg(&so)
                .status()
                .unwrap()
                .success()
        );
        let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
        let handle = crate::os::dll::open_with_flags(
            &c_so,
            crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
        )
        .expect("dlopen hidden/visible probe .so");
        // The hidden symbol misses dlsym; the visible symbol hits
        assert_eq!(crate::os::dll::sym(handle, c"mirvm_hidden_probe"), 0);
        let pvis = crate::os::dll::sym(handle, c"mirvm_visible_probe");
        assert!(pvis != 0);
        // .symtab fallback: the hidden symbol resolves and the call returns the right value
        let syms = symtab_values(so.to_str().unwrap()).unwrap();
        let bias = crate::os::dll::load_bias(handle).expect("load_bias") as u64;
        let hidden_addr = *syms
            .get("mirvm_hidden_probe")
            .expect("symtab contains the hidden symbol")
            + bias;
        let f: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(hidden_addr as usize) };
        assert_eq!(unsafe { f() }, 0x2a);
        // The visible symbol's address must be identical on both paths
        let vis_via_symtab = *syms
            .get("mirvm_visible_probe")
            .expect("symtab contains the visible symbol")
            + bias;
        assert_eq!(vis_via_symtab, pvis as u64);
        // The hidden fallback table = .symtab - .dynsym: hidden is in (carrying archive
        // priority), visible is out (keeping global dlsym resolution, paired with
        // reject_symbol_ambiguity)
        let hidden_only = hidden_symtab_values(so.to_str().unwrap()).unwrap();
        assert_eq!(
            hidden_only.get("mirvm_hidden_probe"),
            syms.get("mirvm_hidden_probe"),
            "the hidden symbol must stay in the fallback table"
        );
        assert!(
            !hidden_only.contains_key("mirvm_visible_probe"),
            "dynsym-visible symbols do not enter the fallback table"
        );
        unsafe { crate::os::dll::close(handle) };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
