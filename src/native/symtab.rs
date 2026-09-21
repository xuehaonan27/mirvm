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
//! from `native::archive`'s constrained link (ELF64 LE x86_64, not stripped). System libraries
//! always have a normal .dynsym and never take this path.
//!
//! The dlopen handle -> load base mapping (dlinfo) lives in `os::dll::load_bias`.

/// Why an ELF symbol table could not be read.
///
/// The enum lives in this file rather than in the tree root because this is the one file of the
/// native layer the TSan harness compiles: the rest of the tree needs the lowering layer, which the
/// harness stubs. `Malformed` is the shape (or truncation), `Io` is the read.
#[derive(Debug, thiserror::Error, serde::Serialize)]
pub(crate) enum Error {
    #[error("{detail}")]
    Malformed { detail: String },

    #[error("{detail}: {source}")]
    Io {
        detail: String,
        #[serde(skip)]
        #[source]
        source: std::io::Error,
    },
}

crate::diag_codes! {
    Error: Native => {
        Malformed => "symtab.malformed",
        Io => "symtab.io",
    }
}

impl Error {
    fn malformed(detail: impl Into<String>) -> Self {
        Error::Malformed {
            detail: detail.into(),
        }
    }

    fn io(detail: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            detail: detail.into(),
            source,
        }
    }
}

use std::collections::HashMap;

use crate::os::obj::ar;
use crate::os::obj::elf::{
    self, SHN_RESERVED, SHN_UNDEF, SHT_DYNSYM, SHT_SYMTAB, STB_GLOBAL, STB_WEAK,
};

/// One `.symtab`/`.dynsym` entry, resolved against its string table.
///
/// `name_offset` is the raw `st_name` and travels with the entry because offset 0 is the string
/// table's empty name: an entry pointing there names nothing and is not a symbol, which is a
/// different question from whether the resolved string is empty.
struct Symbol<'a> {
    name: &'a str,
    name_offset: u32,
    value: u64,
    section_index: u16,
    binding: u8,
}

/// Read entry `index` of `table` (a symbol table section) against `strtab`.
///
/// `str_end` is the string table's end offset, computed once by the caller. `None` means the entry
/// or the name it points at lies outside the image.
fn symbol_at<'a>(
    bytes: &'a [u8],
    table: &elf::Section,
    strtab: &elf::Section,
    str_end: usize,
    index: usize,
) -> Option<Symbol<'a>> {
    let base = usize::try_from(table.offset)
        .ok()?
        .checked_add(index.checked_mul(usize::try_from(table.entsize).ok()?)?)?;
    let name_offset = elf::u32_at(bytes, base)?;
    let binding = bytes.get(base + 4).copied()? >> 4;
    let section_index = elf::u16_at(bytes, base + 6)?;
    let value = elf::u64_at(bytes, base + 8)?;
    let name_start = usize::try_from(strtab.offset)
        .ok()?
        .checked_add(usize::try_from(name_offset).ok()?)?;
    let name_end = name_start
        + bytes
            .get(name_start..str_end.min(bytes.len()))?
            .iter()
            .position(|&b| b == 0)?;
    let name = std::str::from_utf8(bytes.get(name_start..name_end)?).ok()?;
    Some(Symbol {
        name,
        name_offset,
        value,
        section_index,
        binding,
    })
}

/// Resolve a `.so`'s `.symtab`: defined symbol name -> st_value (file virtual address,
/// relative to the load base). Err means it is not the expected ELF64 LE or the structure is
/// out of bounds (corrupted format, which a materialized artifact should never be). Production
/// code only uses hidden_symtab_values; this raw view exists for unit-test comparison (visible
/// symbols must have the same address on both paths).
#[cfg(test)]
pub(crate) fn symtab_values(so_path: &str) -> Result<HashMap<Box<str>, u64>, Error> {
    symbol_table_values(so_path, SHT_SYMTAB)
}

/// The hidden-symbol fallback table = `.symtab` defined minus `.dynsym` defined.
/// dynsym-visible symbols keep their global dlsym resolution (`reject_symbol_ambiguity`
/// already rejects their collision with RTLD_DEFAULT at materialization time); only hidden
/// symbols, unreachable by dlsym, take the "archive before global" link-time binding semantics
/// (see the module header). A parse failure is propagated as Err and every caller degrades to
/// no table.
pub fn hidden_symtab_values(so_path: &str) -> Result<HashMap<Box<str>, u64>, Error> {
    let mut syms = symbol_table_values(so_path, SHT_SYMTAB)?;
    for name in symbol_table_values(so_path, SHT_DYNSYM)?.keys() {
        syms.remove(&**name);
    }
    Ok(syms)
}

/// Resolve the given symbol table section (SHT_SYMTAB / SHT_DYNSYM; entries have the same
/// format): defined symbol name -> st_value (file virtual address, relative to the load base).
fn symbol_table_values(so_path: &str, want_sht: u32) -> Result<HashMap<Box<str>, u64>, Error> {
    let bytes = std::fs::read(so_path)
        .map_err(|e| Error::io(format!("cannot read the shared library `{so_path}`"), e))?;
    let bad = || {
        Error::malformed(format!(
            "archive shared library `{so_path}` is not the expected ELF64 LE (or is corrupted)"
        ))
    };
    let header = elf::FileHeader::parse(&bytes).ok_or_else(bad)?;
    let sections = elf::sections(&bytes, &header).ok_or_else(bad)?;
    for section in &sections {
        if section.ty != want_sht {
            continue;
        }
        if section.entsize < elf::SYM_ENTRY_SIZE as u64 {
            return Err(bad());
        }
        let strtab = sections
            .get(usize::try_from(section.link).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        let str_end = usize::try_from(strtab.offset + strtab.size).map_err(|_| bad())?;
        let count = usize::try_from(section.size / section.entsize.max(1)).map_err(|_| bad())?;
        let mut out = HashMap::new();
        for index in 0..count {
            let symbol = symbol_at(&bytes, section, strtab, str_end, index).ok_or_else(bad)?;
            // Skip SHN_UNDEF (0) and reserved section indices (0xff00+)
            if symbol.name_offset == 0
                || symbol.section_index == SHN_UNDEF
                || symbol.section_index >= SHN_RESERVED
            {
                continue;
            }
            out.insert(Box::from(symbol.name), symbol.value);
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
pub fn archive_undefined_symbols(archive_path: &str) -> Result<Vec<Box<str>>, Error> {
    let bytes = std::fs::read(archive_path).map_err(|e| {
        Error::io(
            format!("cannot read the native archive `{archive_path}`"),
            e,
        )
    })?;
    archive_undefined_symbols_in(&bytes).map_err(|why| {
        Error::malformed(format!(
            "undefined-symbol enumeration failed for static native archive `{archive_path}`: {why}"
        ))
    })
}

/// Undefined-symbol enumeration over the archive's members. The container walk is
/// [`ar::members`]; what is left here is the part that is about symbols: read each ELF member's
/// `.symtab` and keep the GLOBAL/WEAK names with `SHN_UNDEF`.
fn archive_undefined_symbols_in(bytes: &[u8]) -> Result<Vec<Box<str>>, Error> {
    let members = ar::members(bytes).map_err(|why| Error::malformed(why.to_string()))?;
    let mut out: Vec<Box<str>> = Vec::new();
    for member in members {
        if member.starts_with(&elf::IDENT) {
            for symbol in elf_undefined_symbols(member)? {
                if !out.contains(&symbol) {
                    out.push(symbol);
                }
            }
        }
        // A non-ELF member (a text listing, for example) carries no symbols to enumerate.
    }
    Ok(out)
}

/// SHN_UNDEF enumeration over a single ELF64 LE byte slice (GLOBAL/WEAK bindings; the same
/// structural walk as symbol_table_values, but selecting shndx == 0 with no filtering).
fn elf_undefined_symbols(bytes: &[u8]) -> Result<Vec<Box<str>>, Error> {
    let bad = || Error::malformed("not the expected ELF64 LE (or corrupted)");
    let header = elf::FileHeader::parse(bytes).ok_or_else(bad)?;
    let sections = elf::sections(bytes, &header).ok_or_else(bad)?;
    for section in &sections {
        if section.ty != SHT_SYMTAB {
            continue;
        }
        if section.entsize < elf::SYM_ENTRY_SIZE as u64 {
            return Err(bad());
        }
        let strtab = sections
            .get(usize::try_from(section.link).map_err(|_| bad())?)
            .ok_or_else(bad)?;
        let str_end = usize::try_from(strtab.offset + strtab.size).map_err(|_| bad())?;
        let count = usize::try_from(section.size / section.entsize.max(1)).map_err(|_| bad())?;
        let mut out = Vec::new();
        for index in 0..count {
            let symbol = symbol_at(bytes, section, strtab, str_end, index).ok_or_else(bad)?;
            // Take only undefined (SHN_UNDEF) global/weak bindings (LOCAL is the member's
            // internal business)
            if symbol.name_offset == 0
                || symbol.section_index != SHN_UNDEF
                || (symbol.binding != STB_GLOBAL && symbol.binding != STB_WEAK)
            {
                continue;
            }
            out.push(Box::from(symbol.name));
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
        let dir = std::env::temp_dir().join(format!("mirvm-symtab-arundef-{}", std::process::id()));
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

    /// Link an archive with -fvisibility=hidden using `native::archive`'s parameters: its symbols
    /// do not enter .dynsym, but the .symtab fallback must resolve the same address that a
    /// direct dlsym returns.
    #[test]
    fn hidden_symbols_resolve_via_symtab_with_same_address_as_dlsym() {
        let dir = std::env::temp_dir().join(format!("mirvm-symtab-test-{}", std::process::id()));
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
