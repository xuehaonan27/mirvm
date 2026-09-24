//! In-process loading of MC machine-code sections: self-produced `.so` files (the
//! `global_asm`/`dep_asm` family) **without dlopen**.
//!
//! Each phase is its own module: [`parse`] reads the ELF64 headers out of the bytes, [`map`]
//! places the `PT_LOAD` segments in anonymous memory, [`dynamic`] reads the dynamic table,
//! [`symbols`] builds the symbol table, [`relocate`] applies the relocations and [`unwind`] takes
//! the `.eh_frame` records. [`resolve`] closes the chain at priority ① (ahead of RTLD_DEFAULT,
//! the same semantic priority as archive handles: guest-produced objects always beat host libs of
//! the same name).
//!
//! The data source is the raw bytes of the package's MC-section `.so`; this loader has zero
//! dependency on the system linker (kernel mmap/mprotect plus self-parsing, no ld.so concepts).
//! Boundaries, all rejected loudly: non-ET_DYN, an image built for another machine, PT_INTERP,
//! TLS/COPY relocations, non-weak undefined external symbols, STT_GNU_IFUNC — these shapes do not
//! belong to the self-produced global_asm family; encountering one means the `.so` is not our
//! product.

mod dynamic;
mod map;
mod parse;
mod relocate;
mod symbols;
mod unwind;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// The sentence every phase reports for bytes that are not the ELF64 image this loader reads,
/// without saying which structure or field the image got wrong.
fn bad() -> String {
    "MC image is not the expected ELF64 LE DYN (or is corrupt)".to_string()
}

/// A loaded MC image. Symbols are visible only to the Module that holds it; mapping and
/// unwind registration are not undone because external code pointers may still be alive.
#[derive(Debug)]
pub struct McImage {
    mapping: usize,
    load_bias: usize,
    #[allow(dead_code)] // kept for diagnostics (debug prints)
    size: usize,
    /// symbol -> real in-image address (STB_GLOBAL/WEAK and defined; union of hidden and dynsym families)
    pub symbols: HashMap<Box<str>, u64>,
    executable_ranges: Box<[(usize, usize)]>,
    lifecycle: super::native_lifecycle::NativeLifecycle,
    registered_frames: Box<[usize]>,
    committed: AtomicBool,
}

/// MC symbol resolution (priority ① semantics: ahead of the whole process). Image list
/// comes from the current Module; do not search across Engines, otherwise two packages'
/// global_asm symbols would cross-wire.
pub fn resolve(images: &[McImage], name: &str) -> Option<usize> {
    for image in images {
        if let Some(&v) = image.symbols.get(name) {
            return image.load_bias.checked_add(usize::try_from(v).ok()?);
        }
    }
    None
}

/// Load an ELF64 DYN image (raw bytes of a self-produced global_asm/dep_asm family .so).
pub fn load(bytes: &[u8]) -> Result<McImage, String> {
    let image = parse::Image::parse(bytes)?;
    let mapping = map::Mapping::map(bytes, &image)?;
    let names = image.section_names()?;
    let dynamic = dynamic::Dynamic::read(&mapping)?;
    let tables = image.tables(names);
    let symbols = symbols::Symbols::read(&image, &tables, &dynamic)?;
    let registered = symbols.registration_map()?;
    relocate::apply(&mapping, &dynamic, &symbols)?;
    let (initializers, finalizers) = dynamic::lifecycle(&mapping, &dynamic)?;
    let registered_frames = unwind::frames(&mapping, &tables)?;

    mapping.protect()?;
    let executable_ranges = mapping.executable_ranges()?;
    unwind::register(&registered_frames);

    let (mapping, load_bias, size) = mapping.into_parts();
    Ok(McImage {
        mapping,
        load_bias,
        size,
        symbols: registered,
        executable_ranges,
        lifecycle: super::native_lifecycle::NativeLifecycle::new(initializers, finalizers),
        registered_frames,
        committed: AtomicBool::new(false),
    })
}

impl McImage {
    pub(crate) fn load_bias(&self) -> usize {
        self.load_bias
    }

    pub(crate) fn executable_ranges(&self) -> &[(usize, usize)] {
        &self.executable_ranges
    }

    pub(crate) fn commit(&self) {
        self.committed.store(true, Ordering::Release);
    }

    pub(crate) fn run_initializers(&self, args: &mut super::native_lifecycle::InitializerArgs) {
        self.lifecycle.run_initializers(args);
    }

    pub(crate) fn run_finalizers(&self) {
        self.lifecycle.run_finalizers();
    }
}

impl Drop for McImage {
    fn drop(&mut self) {
        if self.committed.load(Ordering::Acquire) {
            return;
        }
        unwind::deregister(&self.registered_frames);
        unsafe { crate::os::mem::unmap(self.mapping as *mut u8, self.size) };
    }
}

#[cfg(test)]
mod tests {
    // This loader reads ELF64 images, and the half that would read this platform's own object
    // format is not written (`crate::os_arch::reloc`'s module doc names the gap). The two fixtures
    // below are therefore an x86_64 ELF built with the ELF toolchain, and the tests that need one
    // run where such an image can be produced; the rest of the module is platform-neutral.
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::native::elf;
    #[cfg(target_os = "linux")]
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(target_os = "linux")]
    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    #[cfg(target_os = "linux")]
    static SIGNAL_NUMBER: AtomicU64 = AtomicU64::new(0);
    #[cfg(target_os = "linux")]
    static SIGNAL_HANDLER: AtomicU64 = AtomicU64::new(0);
    #[cfg(target_os = "linux")]
    static SIGNAL_OWNER: AtomicU64 = AtomicU64::new(0);

    #[cfg(target_os = "linux")]
    extern "C" fn capture_signal(signum: i32, handler: usize, owner: u64) -> usize {
        SIGNAL_NUMBER.store(signum as u64, Ordering::Relaxed);
        SIGNAL_HANDLER.store(handler as u64, Ordering::Relaxed);
        SIGNAL_OWNER.store(owner, Ordering::Relaxed);
        handler
    }

    #[cfg(target_os = "linux")]
    struct FixtureDir(std::path::PathBuf);

    #[cfg(target_os = "linux")]
    impl FixtureDir {
        fn new() -> Self {
            let serial = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "mirvm-mcload-relocation-{}-{serial}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(target_os = "linux")]
    fn relocation_fixture() -> (FixtureDir, Vec<u8>) {
        let directory = FixtureDir::new();
        let source = directory.0.join("probe.S");
        let library = directory.0.join("probe.so");
        std::fs::write(
            &source,
            r#"
.intel_syntax noprefix
.text
.globl absolute_probe
.type absolute_probe,@function
absolute_probe:
    mov rax, QWORD PTR [rip + absolute_pointer]
    ret
.size absolute_probe,.-absolute_probe
.p2align 3
absolute_pointer:
    .quad absolute_target

.globl dynsym_probe
.type dynsym_probe,@function
dynsym_probe:
    mov rax, QWORD PTR [rip + target_value@GOTPCREL]
    mov rax, QWORD PTR [rax]
    ret
.size dynsym_probe,.-dynsym_probe

.globl absolute_target
.set absolute_target, 0x1234
.data
.globl target_value
.type target_value,@object
.size target_value,8
target_value:
    .quad 0x1122334455667788
.section .note.GNU-stack,"",@progbits
"#,
        )
        .unwrap();
        let output = std::process::Command::new("cc")
            .args([
                "-shared",
                "-nostdlib",
                "-fPIC",
                "-Wl,-z,defs",
                "-Wl,-z,notext",
            ])
            .arg(&source)
            .arg("-o")
            .arg(&library)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to build MC relocation fixture:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(library).unwrap();
        (directory, bytes)
    }

    /// The name of section `index`, read straight out of the section-header string table so the
    /// test does not depend on the loader it is testing.
    #[cfg(target_os = "linux")]
    fn section_name_at(
        bytes: &[u8],
        header: &elf::FileHeader,
        sections: &[elf::Section],
        index: usize,
    ) -> String {
        let names = sections[header.shstrndx as usize];
        let start = names.offset as usize + sections[index].name as usize;
        let tail = &bytes[start..];
        let end = tail.iter().position(|&byte| byte == 0).unwrap_or(0);
        String::from_utf8_lossy(&tail[..end]).into_owned()
    }

    /// Writes `value` over the 8-byte field `offset` of the section header `index`.
    #[cfg(target_os = "linux")]
    fn corrupt_section_field(bytes: &mut [u8], index: usize, offset: usize, value: u64) {
        let header = elf::FileHeader::parse(bytes).unwrap();
        let base = header.shoff as usize + index * header.shentsize as usize + offset;
        bytes[base..base + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// A string-table offset past the end of the image is a corrupt image, and the loader says so
    /// rather than slicing out of bounds. Both name readers are reached: the section-header one
    /// through the table search, the symbol one through the first symbol's name.
    #[cfg(target_os = "linux")]
    #[test]
    fn corrupt_string_table_offsets_are_reported_not_panicked_on() {
        let (_directory, bytes) = relocation_fixture();
        let header = elf::FileHeader::parse(&bytes).unwrap();
        let sections = elf::sections(&bytes, &header).unwrap();
        assert!(!sections.is_empty());

        let mut no_section_names = bytes.clone();
        corrupt_section_field(
            &mut no_section_names,
            header.shstrndx as usize,
            elf::shdr::OFFSET,
            u64::MAX / 2,
        );
        let error = load(&no_section_names).unwrap_err();
        assert!(
            error.starts_with("MC image lacks "),
            "a nameless section table must fail as an unsupported image, got: {error}"
        );

        let strtab = (0..sections.len())
            .find(|&index| section_name_at(&bytes, &header, &sections, index) == ".strtab")
            .expect("fixture has a .strtab");
        let mut bad_symbol_names = bytes;
        corrupt_section_field(
            &mut bad_symbol_names,
            strtab,
            elf::shdr::OFFSET,
            u64::MAX / 2,
        );
        let error = load(&bad_symbol_names).unwrap_err();
        assert_eq!(error, "MC image strtab out of bounds");
    }

    #[test]
    fn symbol_lookup_is_scoped_to_the_supplied_images() {
        let image = |base, value| McImage {
            mapping: base,
            load_bias: base,
            size: 0x1000,
            symbols: HashMap::from([(Box::<str>::from("same_symbol"), value)]),
            executable_ranges: Box::new([]),
            lifecycle: Default::default(),
            registered_frames: Box::new([]),
            committed: AtomicBool::new(true),
        };
        let first = image(0x1000, 0x20);
        let second = image(0x4000, 0x80);

        assert_eq!(
            resolve(std::slice::from_ref(&first), "same_symbol"),
            Some(0x1020)
        );
        assert_eq!(
            resolve(std::slice::from_ref(&second), "same_symbol"),
            Some(0x4080)
        );
        assert_eq!(resolve(&[], "same_symbol"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn global_asm_signal_call_uses_the_owned_runtime_bridge() {
        const OWNER: u64 = 0x1020_3040_5060_7080;
        const HANDLER: usize = 0x1234_5678;
        let path = crate::lower::global_asm::assemble(
            r#"
.intel_syntax noprefix
.text
.globl mirvm_mc_signal_bridge_probe
.type mirvm_mc_signal_bridge_probe,@function
mirvm_mc_signal_bridge_probe:
    mov edi, 10
    mov esi, 0x12345678
    jmp signal@PLT
.size mirvm_mc_signal_bridge_probe,.-mirvm_mc_signal_bridge_probe
.section .note.GNU-stack,"",@progbits
"#,
        )
        .unwrap();
        let bytes = std::fs::read(&*path).unwrap();
        let image = load(&bytes).unwrap();
        let patch = |name: &str, value: u64| {
            let offset = image
                .symbols
                .get(name)
                .unwrap_or_else(|| panic!("MC runtime bridge has no `{name}` slot"));
            let address = image
                .load_bias()
                .checked_add(*offset as usize)
                .expect("MC runtime bridge slot address");
            unsafe { (address as *mut u64).write(value) };
        };
        patch("__mirvm_signal_owner", OWNER);
        patch(
            "__mirvm_signal_target",
            capture_signal as *const () as usize as u64,
        );

        let probe: unsafe extern "C" fn() -> usize = unsafe {
            std::mem::transmute(
                resolve(std::slice::from_ref(&image), "mirvm_mc_signal_bridge_probe").unwrap(),
            )
        };
        assert_eq!(unsafe { probe() }, HANDLER);
        assert_eq!(SIGNAL_NUMBER.load(Ordering::Relaxed), 10);
        assert_eq!(SIGNAL_HANDLER.load(Ordering::Relaxed), HANDLER as u64);
        assert_eq!(SIGNAL_OWNER.load(Ordering::Relaxed), OWNER);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dynamic_relocations_use_dynsym_and_run_before_segment_protection() {
        let (_directory, bytes) = relocation_fixture();
        let image = load(&bytes).unwrap();
        let absolute: unsafe extern "C" fn() -> u64 = unsafe {
            std::mem::transmute(
                resolve(std::slice::from_ref(&image), "absolute_probe")
                    .expect("absolute probe symbol"),
            )
        };
        let dynsym: unsafe extern "C" fn() -> u64 = unsafe {
            std::mem::transmute(
                resolve(std::slice::from_ref(&image), "dynsym_probe").expect("dynsym probe symbol"),
            )
        };

        // absolute_pointer is an R_X86_64_64 text relocation against a
        // SHN_ABS dynamic symbol. Applying it after RX protection would fault.
        assert_eq!(unsafe { absolute() }, 0x1234);
        // target_value's GOT relocation indexes .dynsym; the same index in
        // .symtab intentionally denotes a different entry in this fixture.
        assert_eq!(unsafe { dynsym() }, 0x1122_3344_5566_7788);
    }
}
