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
//! from `native::artifact::archive`'s constrained link (ELF64 LE x86_64, not stripped). System
//! libraries always have a normal .dynsym and never take this path.
//!
//! The dlopen handle -> load base mapping (dlinfo) lives in `os::dll::load_bias`.

/// Why an object's symbol table could not be read.
///
/// The enum lives with the readers rather than at the tree root: it is what they raise, and the
/// TSan harness compiles this module tree directly rather than the whole native layer, which needs
/// the lowering layer the harness stubs. `Malformed` is the shape (or truncation), `Io` is the
/// read.
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

use crate::native::object::ar;
use crate::native::object::elf::{SHT_DYNSYM, SHT_SYMTAB};

pub(crate) mod elf;
pub(crate) mod macho;

/// The hidden-symbol fallback table: the symbols an object defines that its loader cannot reach by
/// name.
///
/// A symbol the loader can resolve keeps its global `dlsym` resolution (`reject_symbol_ambiguity`
/// already rejects its collision with `RTLD_DEFAULT` at materialization time); only a symbol
/// `dlsym` cannot reach takes the "archive before global" link-time binding semantics (see the
/// module header).
///
/// Each format spells "the image defines it but nothing can look it up" its own way — an ELF
/// symbol that is in `.symtab` and not in `.dynsym`, a Mach-O one the image marks private — so
/// which format is being read is a parameter. A parse failure is propagated as Err and every
/// caller degrades to no table.
pub fn hidden_symtab_values(
    so_path: &str,
    format: crate::native::object::ObjectFormat,
) -> Result<HashMap<Box<str>, u64>, Error> {
    match format {
        crate::native::object::ObjectFormat::Elf => {
            let mut syms = elf::symbol_table_values(so_path, SHT_SYMTAB)?;
            for name in elf::symbol_table_values(so_path, SHT_DYNSYM)?.keys() {
                syms.remove(&**name);
            }
            Ok(syms)
        }
        crate::native::object::ObjectFormat::MachO => {
            let bytes = std::fs::read(so_path)
                .map_err(|e| Error::io(format!("cannot read the shared library `{so_path}`"), e))?;
            macho::hidden_symbols(&bytes)
                .map(|symbols| symbols.into_iter().collect())
                .map_err(|why| Error::malformed(format!("`{so_path}`: {why}")))
        }
    }
}

// ===== ar archive SHN_UNDEF static enumeration (the "symbol is in the rlib" criterion for
// native-archive closures) =====

/// Static enumeration of a Unix ar archive's undefined symbols: walk member by member
/// (skipping the ar symbol table and long-name table members), read each ELF64 member's
/// `.symtab`, and collect the names of GLOBAL/WEAK symbols with `SHN_UNDEF`. This parses bytes
/// (the same structural walk as `symbol_table_values`) and never parses tool text output.
pub fn archive_undefined_symbols(
    archive_path: &str,
    format: crate::native::object::ObjectFormat,
) -> Result<Vec<Box<str>>, Error> {
    let bytes = std::fs::read(archive_path).map_err(|e| {
        Error::io(
            format!("cannot read the native archive `{archive_path}`"),
            e,
        )
    })?;
    archive_undefined_symbols_in(&bytes, format).map_err(|why| {
        Error::malformed(format!(
            "undefined-symbol enumeration failed for static native archive `{archive_path}`: {why}"
        ))
    })
}

/// Undefined-symbol enumeration over the archive's members. The container walk is
/// [`ar::members`]; what is left here is the part that is about symbols: read each member's symbol
/// table and keep the names it says something outside the archive has to provide.
///
/// The format is a parameter for the same reason [`object_undefined_symbols`]'s is: it is the
/// platform's, because the toolchain that produced the archive is what decided it.
fn archive_undefined_symbols_in(
    bytes: &[u8],
    format: crate::native::object::ObjectFormat,
) -> Result<Vec<Box<str>>, Error> {
    let members = ar::members(bytes).map_err(|why| Error::malformed(why.to_string()))?;
    let mut out: Vec<Box<str>> = Vec::new();
    for member in members {
        let names: Vec<Box<str>> = match format {
            crate::native::object::ObjectFormat::Elf
                if crate::native::object::elf::is_elf64_le(member) =>
            {
                elf::elf_undefined_symbols(member)?
            }
            crate::native::object::ObjectFormat::MachO
                if crate::native::object::macho::is_image(member) =>
            {
                macho::undefined_symbols(member).map_err(|e| Error::malformed(e.to_string()))?
            }
            // A member that is not an object of this format (a text listing, or the archive's own
            // symbol index) carries no symbols to enumerate.
            _ => continue,
        };
        for symbol in names {
            if !out.contains(&symbol) {
                out.push(symbol);
            }
        }
    }
    Ok(out)
}

/// The names a shared object leaves to its loader, in the format its platform's toolchain writes.
///
/// Which names matter is the caller's question — it compares them against the symbols the guest
/// itself defines — so this only answers what the object says. The format is a parameter because
/// it is the platform's: the linker that produced the object is what decided it.
pub(crate) fn object_undefined_symbols(
    path: &str,
    format: crate::native::object::ObjectFormat,
) -> Result<Vec<Box<str>>, Error> {
    let bytes =
        std::fs::read(path).map_err(|error| Error::io(format!("cannot read `{path}`"), error))?;
    match format {
        crate::native::object::ObjectFormat::Elf => elf::elf_undefined_symbols(&bytes),
        crate::native::object::ObjectFormat::MachO => {
            macho::undefined_symbols(&bytes).map_err(|e| Error::malformed(e.to_string()))
        }
    }
}

/// One name an image offers to other images, and whether the format marks the definition weak.
///
/// Weakness is what decides whether two images defining one name is a conflict: two strong
/// definitions leave the choice to load order, while one strong definition wins outright, which is
/// the resolution native static linking would have made.
pub(crate) struct Export {
    pub(crate) name: Box<str>,
    pub(crate) weak: bool,
}

/// The names `path` offers to other images, by the format `format` names.
///
/// Only the offered ones. A symbol the image keeps to itself cannot collide with another image's,
/// because resolution order puts the image's own definition first whatever else the process holds.
pub(crate) fn object_exports(
    path: &str,
    format: crate::native::object::ObjectFormat,
) -> Result<Vec<Export>, Error> {
    let bytes =
        std::fs::read(path).map_err(|error| Error::io(format!("cannot read `{path}`"), error))?;
    match format {
        crate::native::object::ObjectFormat::Elf => elf::elf_exports(&bytes, path),
        crate::native::object::ObjectFormat::MachO => Ok(macho::symbols(&bytes)
            .map_err(|e| Error::malformed(e.to_string()))?
            .into_iter()
            .filter(|symbol| symbol.exported)
            .map(|symbol| Export {
                name: symbol.name,
                weak: symbol.weak,
            })
            .collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::archive_undefined_symbols;
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
        let undef =
            archive_undefined_symbols(a.to_str().unwrap(), crate::os::dll::OBJECT_FORMAT).unwrap();
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

    /// Link an archive with -fvisibility=hidden using `native::artifact::archive`'s parameters: its symbols
    /// do not enter .dynsym, but the .symtab fallback must resolve the same address that a
    /// direct dlsym returns.
    /// The subject is ELF's two tables: a definition the image does not export is absent from
    /// `.dynsym` and present in `.symtab`, and the fallback table is the difference. This format
    /// keeps one table and says "do not offer this" with a bit instead, which is what
    /// `native::symbol::symtab::macho`'s own test covers.
    #[cfg(target_os = "linux")]
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
        let c_so = std::ffi::CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
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
        // The raw `.symtab` view, read through what `hidden_symtab_values` filters: this test
        // exists to compare the two.
        let syms =
            super::elf::symbol_table_values(so.to_str().unwrap(), super::SHT_SYMTAB).unwrap();
        let bias = crate::os::dll::load_bias(handle, &c_so).expect("load_bias") as u64;
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
        // The probe this test builds is an ELF object whatever the host is, so it asks as one.
        let hidden_only = super::hidden_symtab_values(
            so.to_str().unwrap(),
            crate::native::object::ObjectFormat::Elf,
        )
        .unwrap();
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
