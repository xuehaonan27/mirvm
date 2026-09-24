//! Where a produced object's constructors and destructors are, and taking the loader's hands off
//! them.
//!
//! A platform's runtime runs an object's constructors before any code in it and its destructors at
//! teardown. mirvm's images are mapped once for the process but their language lifecycle belongs to
//! the Engine that loaded them, so the load phase reads the addresses out of the object and then
//! removes the loader's ownership of them.
//!
//! Two formats say the same two things differently, and the difference is a layout one, which is
//! why it is here rather than on an axis. ELF names a singular `DT_INIT`/`DT_FINI` and two arrays in
//! the dynamic table, and the loader is told to forget them by rewriting those tags. Mach-O has no
//! such table: the lists are sections whose *type* is what makes them lists, and the loader is told
//! to forget them by taking that type away. Mach-O also writes the initializer list in either of
//! two forms — the pointer array an old linker emitted and the section of 32-bit offsets a modern
//! one emits instead — which is [`CallableList`] rather than two more fields.
//!
//! What the addresses mean once read — that a list becomes callables after the image is mapped and
//! slid — is not a layout question and stays with the caller. So does the platform's signer:
//! rewriting these bytes invalidates an image's signature, and putting the file back in front of
//! the signer is `crate::os::dll::reseal`'s job rather than this module's.
//!
//! Measured on the macos aarch64 host, because the Mach-O points here are easy to get wrong: a
//! pointer list is one dyld *rebases*, so zeroing its contents yields the slide rather than null
//! and the process dies on the first "constructor"; a rewritten image that is not signed again is
//! killed outright; and this toolchain emits no pointer array for a constructor at all, so a port
//! that read only that form ran every constructor at `dlopen`, before anything could be wired.

use std::path::Path;

use super::{elf, macho};
use crate::os::dll::ObjectFormat;

/// How a list of callables is written down in an object.
///
/// ELF has one form, an array of pointers the loader relocates. Mach-O has two, because a modern
/// ld64 emits no pointer array for the initializers: it emits a section of 32-bit offsets from the
/// image's base, which the loader turns into an address by adding the load address rather than by
/// rebasing the entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallableList {
    /// Pointers the loader rebases, one per callable.
    Pointers,
    /// 32-bit offsets from the image's base, one per callable.
    Offsets,
}

/// One image's lifecycle metadata, in the virtual addresses it is linked at.
///
/// Each list is `(address, size)` of a callable list, which only becomes a list of callables after
/// the image is mapped; the singular addresses are single functions. Both are the object's own
/// linked addresses, so a caller adds the load bias.
#[derive(Default)]
pub(crate) struct Layout {
    pub(crate) init: Option<u64>,
    pub(crate) init_array: Option<(u64, u64, CallableList)>,
    pub(crate) fini: Option<u64>,
    pub(crate) fini_array: Option<(u64, u64, CallableList)>,
    pub(crate) loads: Vec<(u64, u64)>,
    pub(crate) executable_loads: Vec<(u64, u64)>,
}

/// Read `path`'s lifecycle metadata and take the loader's hands off it, returning what was there.
///
/// The file is rewritten, so its signature no longer covers it: a caller on a platform whose loader
/// checks one must hand the path to `crate::os::dll::reseal` before mapping it again.
pub(crate) fn read_and_suppress(path: &Path, format: ObjectFormat) -> Result<Layout, String> {
    let mut bytes = std::fs::read(path)
        .map_err(|error| format!("fail to read private native `{}`: {error}", path.display()))?;
    let layout = match format {
        ObjectFormat::Elf => elf_layout(&mut bytes, path)?,
        ObjectFormat::MachO => macho_layout(&mut bytes, path)?,
    };
    std::fs::write(path, &bytes).map_err(|error| {
        format!(
            "fail to suppress native lifecycle `{}`: {error}",
            path.display()
        )
    })?;
    Ok(layout)
}

/// The ELF half: two arrays and two singular functions in the dynamic table, each named by a tag
/// that is rewritten once its value has been read.
fn elf_layout(bytes: &mut [u8], path: &Path) -> Result<Layout, String> {
    let bad = || format!("native `{}` is not valid ELF64 LE", path.display());
    let header = elf::FileHeader::parse(bytes).ok_or_else(bad)?;
    if header.kind != elf::ET_DYN || header.machine != crate::arch::ELF_MACHINE {
        return Err(bad());
    }
    let phoff = usize::try_from(header.phoff).map_err(|_| bad())?;
    let phentsize = usize::from(header.phentsize);
    let phnum = usize::from(header.phnum);
    if phentsize < elf::PHDR_SIZE {
        return Err(bad());
    }
    let mut dynamic = None;
    let mut result = Layout::default();
    for index in 0..phnum {
        let base = phoff
            .checked_add(index.checked_mul(phentsize).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        let ty = elf::u32_at(bytes, base + elf::phdr::TYPE).ok_or_else(bad)?;
        let flags = elf::u32_at(bytes, base + elf::phdr::FLAGS).ok_or_else(bad)?;
        let off = elf::u64_at(bytes, base + elf::phdr::OFFSET).ok_or_else(bad)?;
        let vaddr = elf::u64_at(bytes, base + elf::phdr::VADDR).ok_or_else(bad)?;
        let filesz = elf::u64_at(bytes, base + elf::phdr::FILESZ).ok_or_else(bad)?;
        let memsz = elf::u64_at(bytes, base + elf::phdr::MEMSZ).ok_or_else(bad)?;
        if ty == elf::PT_LOAD {
            let end = vaddr
                .checked_add(memsz)
                .ok_or_else(|| format!("native `{}` load range overflow", path.display()))?;
            result.loads.push((vaddr, end));
            if flags & elf::PF_X != 0 {
                result.executable_loads.push((vaddr, end));
            }
        } else if ty == elf::PT_DYNAMIC {
            dynamic = Some((off, filesz));
        }
    }
    let Some((dynamic_off, dynamic_size)) = dynamic else {
        return Err(format!("native `{}` has no PT_DYNAMIC", path.display()));
    };
    let start = usize::try_from(dynamic_off).map_err(|_| bad())?;
    let size = usize::try_from(dynamic_size).map_err(|_| bad())?;
    let end = start.checked_add(size).ok_or_else(bad)?;
    if end > bytes.len() || !size.is_multiple_of(elf::DYN_ENTRY_SIZE) {
        return Err(bad());
    }

    let mut init_array_addr = None;
    let mut init_array_size = None;
    let mut fini_array_addr = None;
    let mut fini_array_size = None;
    for entry in (start..end).step_by(elf::DYN_ENTRY_SIZE) {
        let tag = i64::from_le_bytes(bytes[entry..entry + 8].try_into().map_err(|_| bad())?);
        if tag == elf::DT_NULL {
            break;
        }
        let value = u64::from_le_bytes(bytes[entry + 8..entry + 16].try_into().map_err(|_| bad())?);
        match tag {
            elf::DT_INIT => result.init = Some(value),
            elf::DT_FINI => result.fini = Some(value),
            elf::DT_INIT_ARRAY => init_array_addr = Some(value),
            elf::DT_INIT_ARRAYSZ => init_array_size = Some(value),
            elf::DT_FINI_ARRAY => fini_array_addr = Some(value),
            elf::DT_FINI_ARRAYSZ => fini_array_size = Some(value),
            elf::DT_PREINIT_ARRAY | elf::DT_PREINIT_ARRAYSZ => {
                return Err(format!(
                    "native `{}` unexpectedly contains a preinit array",
                    path.display()
                ));
            }
            _ => continue,
        }
        // DT_BIND_NOW is a harmless boolean tag under the already requested
        // RTLD_NOW mode. Replacing lifecycle tags removes them from l_info
        // without terminating the dynamic table early or changing relocation.
        bytes[entry..entry + 8].copy_from_slice(&elf::DT_BIND_NOW.to_le_bytes());
        bytes[entry + 8..entry + 16].fill(0);
    }
    result.init_array = pair_tags(init_array_addr, init_array_size, "DT_INIT_ARRAY")?
        .map(|(address, size)| (address, size, CallableList::Pointers));
    result.fini_array = pair_tags(fini_array_addr, fini_array_size, "DT_FINI_ARRAY")?
        .map(|(address, size)| (address, size, CallableList::Pointers));
    Ok(result)
}

/// The Mach-O half: the lists are sections the loader runs because of their type, so the read is a
/// section walk and the suppression is that type ceasing to say so.
fn macho_layout(bytes: &mut [u8], path: &Path) -> Result<Layout, String> {
    let bad = |detail: String| format!("native `{}`: {detail}", path.display());
    let image = macho::image(bytes).map_err(bad)?;
    let mut result = Layout::default();
    for segment in &image.segments {
        if segment.vmsize == 0 {
            continue;
        }
        let end = segment
            .vmaddr
            .checked_add(segment.vmsize)
            .ok_or_else(|| bad("segment range overflow".to_string()))?;
        result.loads.push((segment.vmaddr, end));
        if segment.is_executable() {
            result.executable_loads.push((segment.vmaddr, end));
        }
    }
    result.init = image.routines_init;
    for segment in &image.segments {
        for section in &segment.sections {
            let Some(form) = section.callable_list() else {
                continue;
            };
            // The offsets form exists for the initializers alone; a name is what separates the two
            // pointer lists from each other.
            let array = if form == CallableList::Offsets || section.sectname == "__mod_init_func" {
                &mut result.init_array
            } else if section.sectname == "__mod_term_func" {
                &mut result.fini_array
            } else {
                // A typed list under no name this format uses is one whose destructors or
                // constructors mirvm would otherwise leave to the loader after patching nothing.
                return Err(bad(format!(
                    "section `{},{}` is a callable list this port does not recognise",
                    section.segname, section.sectname
                )));
            };
            if array.replace((section.addr, section.size, form)).is_some() {
                return Err(bad(format!("two `{}` sections", section.sectname)));
            }
            section.detach(bytes)?;
        }
    }
    Ok(result)
}

fn pair_tags(
    address: Option<u64>,
    size: Option<u64>,
    what: &str,
) -> Result<Option<(u64, u64)>, String> {
    match (address, size) {
        (None, None) => Ok(None),
        (Some(address), Some(size)) => Ok(Some((address, size))),
        _ => Err(format!("native has incomplete {what} metadata")),
    }
}

/// The Mach-O half is one this build cannot check by reading its own writer, so the test links a
/// real dylib with a real constructor and asks this platform's own loader whether it still runs.
///
/// The two runs of the same image are the whole point: the unsuppressed copy proves the
/// constructor is one this loader would have run, so the suppressed copy staying silent is the
/// suppression and not an image that never had a constructor.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::path::Path;

    use super::{CallableList, ObjectFormat, read_and_suppress};
    use crate::native::macho;

    /// A constructor that appends to the file `PROBE_MARKER` names, an exported function, and a
    /// destructor the test does not observe because it would run at process exit.
    const SOURCE: &str = r#"
#include <stdio.h>
#ifndef PROBE_MARKER
#error "PROBE_MARKER must name a file"
#endif
__attribute__((constructor)) static void probe_ctor(void) {
    FILE *f = fopen(PROBE_MARKER, "a");
    if (f) { fputs("init\n", f); fclose(f); }
}
int probe_export(int x) { return x + 1; }
"#;

    fn run(command: &mut std::process::Command) {
        let output = command.output().expect("launching a toolchain");
        assert!(
            output.status.success(),
            "{command:?} failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Links `SOURCE` into a dylib carrying a constructor that reports itself through `marker`.
    /// The archive step is not decoration: it is the shape `native::archive` builds.
    ///
    /// `classic` picks which of the two initializer forms this loader will see: a linker run
    /// without fixup chains writes the pointer array, and the default one writes the offset list
    /// instead. Both are real images this platform runs, so both are what the suppression must
    /// reach.
    fn linked(dir: &Path, marker: &Path, classic: bool) -> std::path::PathBuf {
        let source = dir.join("probe.c");
        std::fs::write(&source, SOURCE).expect("writing the probe source");
        let object = dir.join("probe.o");
        run(std::process::Command::new("cc")
            .args(["-fPIC", "-c"])
            .arg(format!("-DPROBE_MARKER=\"{}\"", marker.display()))
            .arg("-o")
            .arg(&object)
            .arg(&source));
        let archive = dir.join("libprobe.a");
        run(std::process::Command::new("ar")
            .arg("crs")
            .arg(&archive)
            .arg(&object));
        let library = if classic {
            dir.join("classic.so")
        } else {
            dir.join("offsets.so")
        };
        let mut command = std::process::Command::new("cc");
        command.args(["-shared", "-fPIC"]);
        if classic {
            command.arg("-Wl,-no_fixup_chains");
        }
        run(command
            .arg("-Wl,-all_load")
            .arg("-o")
            .arg(&library)
            .arg(&archive));
        library
    }

    fn load(path: &Path) {
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("a path");
        crate::os::dll::open_with_flags(
            &cpath,
            crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
        )
        .expect("a dylib this platform's toolchain signed");
    }

    fn marker(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Builds the control image, suppresses a private copy of it, and reports what the copy's
    /// initializer list was read as. The two runs of the same image are the whole point: the
    /// unsuppressed copy proves the constructor is one this loader would have run, so the
    /// suppressed copy staying silent is the suppression and not an image that never had one.
    fn suppression_probe(classic: bool, expected: CallableList) {
        let dir = std::env::temp_dir().join(format!(
            "mirvm-macho-lifecycle-{}-{classic}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        let control_marker = dir.join("control.marker");
        let control = linked(&dir, &control_marker, classic);
        load(&control);
        assert_eq!(
            marker(&control_marker),
            "init\n",
            "the control image's constructor must run, or the test proves nothing"
        );

        let suppressed_marker = dir.join("suppressed.marker");
        let suppressed = dir.join("suppressed.so");
        std::fs::copy(&control, &suppressed).expect("a private copy");
        let layout = read_and_suppress(&suppressed, ObjectFormat::MachO).expect("a dylib");
        crate::os::dll::reseal(&suppressed).expect("the signer accepting the rewritten copy");

        let (address, size, form) = layout.init_array.expect("the initializer list");
        assert_eq!(
            form, expected,
            "the linker wrote the other initializer form"
        );
        assert_eq!(size, if classic { 8 } else { 4 });
        assert!(address != 0);
        assert!(!layout.executable_loads.is_empty());
        let bytes = std::fs::read(&suppressed).expect("the rewritten copy");
        let image = macho::image(&bytes).expect("still a dylib");
        assert!(
            !image
                .segments
                .iter()
                .flat_map(|segment| &segment.sections)
                .any(|section| section.callable_list().is_some()),
            "the list must no longer be typed as one the loader runs"
        );

        load(&suppressed);
        assert_eq!(
            marker(&suppressed_marker),
            "",
            "the suppressed constructor still ran"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_suppressed_pointer_constructor_is_one_the_loader_no_longer_runs() {
        suppression_probe(true, CallableList::Pointers);
    }

    /// The default this platform's toolchain links with, and the form that made a real fixture run
    /// its constructor at `dlopen` before mirvm could wire the slot the constructor calls.
    #[test]
    fn a_suppressed_offset_constructor_is_one_the_loader_no_longer_runs() {
        suppression_probe(false, CallableList::Offsets);
    }
}
