use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use super::archive::{materialize_for_target_in, materialize_in, reject_symbol_ambiguity};

use crate::diag::Diagnostic as _;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
static SIGNAL_SIGNUM: AtomicU64 = AtomicU64::new(0);
static SIGNAL_HANDLER: AtomicU64 = AtomicU64::new(0);
static SIGNAL_OWNER: AtomicU64 = AtomicU64::new(0);
static SIGACTION_SIGNUM: AtomicU64 = AtomicU64::new(0);
static SIGACTION_OWNER: AtomicU64 = AtomicU64::new(0);
static RAISE_SIGNUM: AtomicU64 = AtomicU64::new(0);
static RAISE_OWNER: AtomicU64 = AtomicU64::new(0);

extern "C" fn test_native_signal(signum: i32, handler: usize, owner: u64) -> usize {
    SIGNAL_SIGNUM.store(signum as u64, Ordering::Relaxed);
    SIGNAL_HANDLER.store(handler as u64, Ordering::Relaxed);
    SIGNAL_OWNER.store(owner, Ordering::Relaxed);
    handler
}

extern "C" fn test_native_sigaction(
    signum: i32,
    _act: *const libc::c_void,
    _oldact: *mut libc::c_void,
    owner: u64,
) -> i32 {
    SIGACTION_SIGNUM.store(signum as u64, Ordering::Relaxed);
    SIGACTION_OWNER.store(owner, Ordering::Relaxed);
    71
}

extern "C" fn test_native_raise(signum: i32, owner: u64) -> i32 {
    RAISE_SIGNUM.store(signum as u64, Ordering::Relaxed);
    RAISE_OWNER.store(owner, Ordering::Relaxed);
    73
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mirvm-native-archive-test-{name}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn make_archive(dir: &Path, source: &str) -> PathBuf {
    let source_path = dir.join("probe.c");
    let object_path = dir.join("probe.o");
    let archive_path = dir.join("libprobe.a");
    std::fs::write(&source_path, source).unwrap();
    let cc = Command::new("cc")
        .args(["-fPIC", "-c"])
        .arg(&source_path)
        .arg("-o")
        .arg(&object_path)
        .status()
        .unwrap();
    assert!(cc.success());
    let ar = Command::new("ar")
        .arg("crs")
        .arg(&archive_path)
        .arg(&object_path)
        .status()
        .unwrap();
    assert!(ar.success());
    archive_path
}

#[cfg(target_os = "linux")]
fn make_thin_archive(dir: &Path, source: &str) -> PathBuf {
    let source_path = dir.join("thin_probe.c");
    let object_path = dir.join("thin_probe.o");
    let archive_path = dir.join("libthin_probe.a");
    std::fs::write(&source_path, source).unwrap();
    let cc = Command::new("cc")
        .args(["-fPIC", "-c"])
        .arg(&source_path)
        .arg("-o")
        .arg(&object_path)
        .status()
        .unwrap();
    assert!(cc.success());
    let ar = Command::new("ar")
        .arg("crsT")
        .arg(&archive_path)
        .arg(&object_path)
        .status()
        .unwrap();
    assert!(ar.success());
    archive_path
}

#[cfg(target_arch = "x86_64")]
fn make_non_pic_archive(dir: &Path, source: &str) -> PathBuf {
    let source_path = dir.join("non_pic_probe.c");
    let object_path = dir.join("non_pic_probe.o");
    let archive_path = dir.join("libnon_pic_probe.a");
    std::fs::write(&source_path, source).unwrap();
    let cc = Command::new("cc")
        // Force an actually absolute text relocation. On x86-64 the default
        // small model emits PC-relative code even without `-fPIC`; -Bsymbolic
        // can resolve that code safely inside the private Engine image.
        .args(["-fno-pic", "-mcmodel=large", "-c"])
        .arg(&source_path)
        .arg("-o")
        .arg(&object_path)
        .status()
        .unwrap();
    assert!(cc.success());
    let ar = Command::new("ar")
        .arg("crs")
        .arg(&archive_path)
        .arg(&object_path)
        .status()
        .unwrap();
    assert!(ar.success());
    archive_path
}

fn make_cc_wrapper(dir: &Path, name: &str, version: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version}'; exit 0; fi\nexec cc \"$@\"\n"
            ),
        )
        .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

#[test]
fn unreferenced_archive_symbol_is_dlsym_visible() {
    let temp = TempDir::new("tracer");
    let archive = make_archive(
        temp.path(),
        "unsigned long mirvm_archive_probe(void) { return 0x51aUL; }\n",
    );

    let so = materialize_in(&archive, &temp.path().join("cache")).unwrap();
    let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
    let handle = crate::os::dll::open_with_flags(
        &c_so,
        crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
    )
    .unwrap_or_else(|e| panic!("dlopen {} failed: {}", so.display(), e));
    let address = crate::os::dll::sym(handle, c"mirvm_archive_probe");
    assert!(address != 0, "whole-archive did not export tracer symbol");
    let probe: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(address as *const u8) };
    assert_eq!(unsafe { probe() }, 0x51a);
    unsafe { crate::os::dll::close(handle) };
}

#[test]
fn native_signal_calls_receive_the_engine_owner() {
    const OWNER: u64 = 0x0ddc_0ffe_e15e_c7ed;
    const HANDLER: usize = 0x1234_5678;

    let temp = TempDir::new("signal-bridge");
    let archive = make_archive(
        temp.path(),
        "#include <signal.h>\n\
             void *mirvm_call_signal(int signum, void *handler) {\n\
                 return (void *)signal(signum, (void (*)(int))handler);\n\
             }\n\
             int mirvm_call_sigaction(int signum) { return sigaction(signum, 0, 0); }\n\
             int mirvm_call_raise(int signum) { return raise(signum); }\n",
    );

    let so = materialize_in(&archive, &temp.path().join("cache")).unwrap();
    let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
    let handle = crate::os::dll::open_with_flags(
        &c_so,
        crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
    )
    .unwrap_or_else(|e| panic!("dlopen {} failed: {e}", so.display()));
    let bias = crate::os::dll::load_bias(handle, &c_so).expect("native bridge load bias") as u64;
    // The slots are reached the two ways the engine reaches them, because the platform decides
    // whether the bridge exports them: through the loader, which searches an image's whole load
    // closure, and through the image's own table where they are not exported.
    let hidden = crate::native::symtab::hidden_symtab_values(
        so.to_str().unwrap(),
        crate::os::dll::OBJECT_FORMAT,
    )
    .unwrap();
    let patch = |name: &str, value: u64| {
        let cname = CString::new(name).unwrap();
        let exported = crate::os::dll::sym(handle, &cname);
        let address = if exported != 0 {
            exported
        } else {
            let offset = hidden
                .get(name)
                .unwrap_or_else(|| panic!("native bridge has no slot `{name}`"));
            bias.checked_add(*offset)
                .expect("native bridge slot address") as usize
        };
        unsafe { (address as *mut u64).write(value) };
    };
    patch("__mirvm_signal_owner", OWNER);
    patch(
        "__mirvm_signal_target",
        test_native_signal as *const () as usize as u64,
    );
    patch(
        "__mirvm_sigaction_target",
        test_native_sigaction as *const () as usize as u64,
    );
    patch(
        "__mirvm_raise_target",
        test_native_raise as *const () as usize as u64,
    );

    let signal: unsafe extern "C" fn(i32, usize) -> usize =
        unsafe { std::mem::transmute(crate::os::dll::sym(handle, c"mirvm_call_signal")) };
    let sigaction: unsafe extern "C" fn(i32) -> i32 =
        unsafe { std::mem::transmute(crate::os::dll::sym(handle, c"mirvm_call_sigaction")) };
    let raise: unsafe extern "C" fn(i32) -> i32 =
        unsafe { std::mem::transmute(crate::os::dll::sym(handle, c"mirvm_call_raise")) };

    assert_eq!(unsafe { signal(1234, HANDLER) }, HANDLER);
    assert_eq!(SIGNAL_SIGNUM.load(Ordering::Relaxed), 1234);
    assert_eq!(SIGNAL_HANDLER.load(Ordering::Relaxed), HANDLER as u64);
    assert_eq!(SIGNAL_OWNER.load(Ordering::Relaxed), OWNER);
    assert_eq!(unsafe { sigaction(1235) }, 71);
    assert_eq!(SIGACTION_SIGNUM.load(Ordering::Relaxed), 1235);
    assert_eq!(SIGACTION_OWNER.load(Ordering::Relaxed), OWNER);
    assert_eq!(unsafe { raise(1236) }, 73);
    assert_eq!(RAISE_SIGNUM.load(Ordering::Relaxed), 1236);
    assert_eq!(RAISE_OWNER.load(Ordering::Relaxed), OWNER);

    unsafe { crate::os::dll::close(handle) };
}

/// `ar crsT` on this host leaves an ordinary archive -- measured, the bytes still begin with
/// `!<arch>\n` -- so the fixture cannot be produced here. The check it exercises is a prefix test
/// on the container's own signature, which is the same bytes on every platform.
#[cfg(target_os = "linux")]
#[test]
fn thin_archive_is_rejected_because_its_content_hash_is_incomplete() {
    let temp = TempDir::new("thin");
    let archive = make_thin_archive(
        temp.path(),
        "unsigned long mirvm_thin_probe(void) { return 7UL; }\n",
    );

    let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
    assert_eq!(error.code(), Some("archive.unsupported"));
    assert!(
        error.to_string().contains("thin"),
        "unexpected diagnostic: {error}"
    );
}

#[test]
fn constructor_runs_at_dlopen_after_lifecycle_guard_is_lifted() {
    // §7.8: lifecycle guard decoded—dlopen's DT_INIT_ARRAY semantics = native process-startup
    // constructor (two proven cases: aws-lc do_library_init / mimalloc mi_process_attach);
    // mirvm never dlcloses, so fini has no observable side (same as native exit being reclaimed by the OS).
    let temp = TempDir::new("constructor-accepted");
    let archive = make_archive(
        temp.path(),
        "static unsigned long mirvm_flag;\n\
             static void boot(void) __attribute__((constructor));\n\
             static void boot(void) { mirvm_flag = 42UL; }\n\
             unsigned long mirvm_constructor_probe(void) { return mirvm_flag; }\n",
    );

    let so = materialize_in(&archive, &temp.path().join("cache")).unwrap();
    let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
    let handle = crate::os::dll::open_with_flags(
        &c_so,
        crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
    )
    .unwrap_or_else(|_| panic!("dlopen {} failed", so.display()));
    let address = crate::os::dll::sym(handle, c"mirvm_constructor_probe");
    assert!(address != 0);
    let probe: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(address as *const u8) };
    assert_eq!(
        unsafe { probe() },
        42,
        "DT_INIT_ARRAY must have already executed at dlopen time (constructor set it to 42)"
    );
    unsafe { crate::os::dll::close(handle) };
}

/// `.init`/`.fini` sections are an ELF practice and this platform's assembler refuses the section
/// name outright, so the fixture cannot exist here. The check reads whatever sections a member
/// carries and is a no-op on a format that has none.
#[cfg(target_os = "linux")]
#[test]
fn legacy_elf_init_and_fini_sections_are_rejected() {
    // §7.8 partition: legacy `.init`/`.fini` sections remain rejected (old gcc trick of injecting
    // bare function bodies, unreliable execution semantics—measured in-repo as SIGSEGV during dlopen);
    // .init_array family is allowed (see constructor_runs_at_dlopen_after_lifecycle_guard_is_lifted).
    let temp = TempDir::new("legacy-init-fini");
    for section in [".init", ".fini"] {
        let dir = temp.path().join(section.trim_start_matches('.'));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = make_archive(
            &dir,
            &format!(
                "__attribute__((used, section(\"{section}\"))) \
                     void mirvm_lifecycle_hook(void) {{}}\n"
            ),
        );

        let error = materialize_in(&archive, &dir.join("cache")).unwrap_err();
        assert_eq!(error.code(), Some("archive.unsupported"));
        assert!(
            error.to_string().contains("`.init`/`.fini`"),
            "section {section} was not rejected: {error}"
        );
    }
}

#[test]
fn dot_init_in_archive_path_is_not_mistaken_for_a_section() {
    let temp = TempDir::new("section-parser");
    let dir = temp.path().join("ordinary.init.path");
    std::fs::create_dir_all(&dir).unwrap();
    let archive = make_archive(
        &dir,
        "unsigned long mirvm_not_a_constructor(void) { return 23UL; }\n",
    );

    materialize_in(&archive, &dir.join("cache"))
        .expect("`.init` in a path must not be parsed as an ELF section");
}

#[test]
fn cache_key_separates_target_triples() {
    let temp = TempDir::new("target-key");
    let archive = make_archive(
        temp.path(),
        "unsigned long mirvm_target_key_probe(void) { return 11UL; }\n",
    );
    let cache = temp.path().join("cache");

    let first = materialize_for_target_in(
        &archive,
        &cache,
        "x86_64-unknown-linux-gnu",
        Path::new("cc"),
        &[],
        None,
    )
    .unwrap();
    let second = materialize_for_target_in(
        &archive,
        &cache,
        "aarch64-unknown-linux-gnu",
        Path::new("cc"),
        &[],
        None,
    )
    .unwrap();
    assert_ne!(first, second, "target triple must participate in cache key");
}

#[test]
fn unresolved_archive_dependency_never_becomes_a_usable_image() {
    let temp = TempDir::new("unresolved");
    let archive = make_archive(
        temp.path(),
        "extern unsigned long mirvm_missing_dependency(void);\n\
             unsigned long mirvm_dependency_probe(void) { return mirvm_missing_dependency(); }\n",
    );

    // Which layer refuses depends on whether the platform can refuse at link time at all. One that
    // cannot -- its linker is told to leave undefined references for the loader -- is caught one
    // step later, and that is the guarantee worth stating: an archive with an open dependency never
    // becomes an image the engine can use.
    match materialize_in(&archive, &temp.path().join("cache")) {
        Err(error) => {
            // The conversion refusal covers every reason a member cannot join a shared library
            // (non-PIC and an open dependency alike); the linker's own output is the detail.
            assert_eq!(error.code(), Some("archive.unsupported"));
            assert!(
                error.to_string().contains("dependency"),
                "unexpected diagnostic: {error}"
            );
            assert!(
                error.to_string().contains("mirvm_missing_dependency"),
                "linker detail lost: {error}"
            );
        }
        Ok(so) => {
            let c_so = CString::new(so.as_os_str().as_encoded_bytes()).unwrap();
            assert!(
                crate::os::dll::open_with_flags(
                    &c_so,
                    crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
                )
                .is_err(),
                "`{}` loaded although it has an unresolved dependency",
                so.display()
            );
        }
    }
}

/// Only where a C compiler can emit a non-PIC object at all: this CPU's code is PC-relative by
/// construction, so its relocations are never the absolute ones the check rejects.
#[cfg(target_arch = "x86_64")]
#[test]
fn non_pic_archive_fails_during_materialization() {
    let temp = TempDir::new("non-pic");
    let archive = make_non_pic_archive(
        temp.path(),
        "unsigned long mirvm_non_pic_global = 13UL;\n\
             unsigned long mirvm_non_pic_probe(void) { return mirvm_non_pic_global; }\n",
    );

    let error = materialize_in(&archive, &temp.path().join("cache")).unwrap_err();
    assert_eq!(error.code(), Some("archive.unsupported"));
    assert!(
        error.to_string().contains("PIC"),
        "unexpected diagnostic: {error}"
    );
    assert!(
        error.to_string().contains("relocation"),
        "linker detail lost: {error}"
    );
}

#[test]
fn cache_key_separates_c_compiler_identities() {
    let temp = TempDir::new("cc-key");
    let archive = make_archive(
        temp.path(),
        "unsigned long mirvm_cc_key_probe(void) { return 17UL; }\n",
    );
    let cc_a = make_cc_wrapper(temp.path(), "cc-a", "mirvm test cc A");
    let cc_b = make_cc_wrapper(temp.path(), "cc-b", "mirvm test cc B");
    let cache = temp.path().join("cache");

    let first = materialize_for_target_in(
        &archive,
        &cache,
        "x86_64-unknown-linux-gnu",
        &cc_a,
        &[],
        None,
    )
    .unwrap();
    let second = materialize_for_target_in(
        &archive,
        &cache,
        "x86_64-unknown-linux-gnu",
        &cc_b,
        &[],
        None,
    )
    .unwrap();
    assert_ne!(
        first, second,
        "C compiler identity must participate in cache key"
    );
}

#[test]
fn cache_key_separates_extra_libs() {
    let temp = TempDir::new("extra-libs-key");
    let archive = make_archive(
        temp.path(),
        "unsigned long mirvm_extra_libs_probe(void) { return 29UL; }\n",
    );
    let cache = temp.path().join("cache");
    let none: &[Box<str>] = &[];
    let with_m: &[Box<str>] = &["m".into()];
    let first = materialize_for_target_in(
        &archive,
        &cache,
        "x86_64-unknown-linux-gnu",
        Path::new("cc"),
        none,
        None,
    )
    .unwrap();
    let second = materialize_for_target_in(
        &archive,
        &cache,
        "x86_64-unknown-linux-gnu",
        Path::new("cc"),
        with_m,
        None,
    )
    .unwrap();
    assert_ne!(
        first, second,
        "extra libs list must participate in cache key"
    );
}

#[test]
fn duplicate_symbols_across_archives_are_rejected_as_link_order_ambiguity() {
    let temp = TempDir::new("duplicate-symbol");
    let first_dir = temp.path().join("first");
    let second_dir = temp.path().join("second");
    std::fs::create_dir_all(&first_dir).unwrap();
    std::fs::create_dir_all(&second_dir).unwrap();
    let first_archive = make_archive(
        &first_dir,
        "unsigned long mirvm_duplicate_symbol(void) { return 1UL; }\n",
    );
    let second_archive = make_archive(
        &second_dir,
        "unsigned long mirvm_duplicate_symbol(void) { return 2UL; }\n",
    );
    let cache = temp.path().join("cache");
    let first = materialize_for_target_in(
        &first_archive,
        &cache,
        "x86_64-unknown-linux-gnu",
        Path::new("cc"),
        &[],
        None,
    )
    .unwrap();
    let second = materialize_for_target_in(
        &second_archive,
        &cache,
        "x86_64-unknown-linux-gnu",
        Path::new("cc"),
        &[],
        None,
    )
    .unwrap();

    let error = reject_symbol_ambiguity(&[first, second]).unwrap_err();
    assert_eq!(error.code(), Some("archive.ambiguous"));
    assert!(
        error.to_string().contains("mirvm_duplicate_symbol"),
        "unexpected diagnostic: {error}"
    );
    assert!(
        error.to_string().contains("order"),
        "unexpected diagnostic: {error}"
    );
}

/// weak/COMDAT semantics (proven by c_risc0_run): duplicate symbols across archives—
/// all-weak allowed (first wins); exactly one strong + weak allowed (strong wins); two strong remain rejected.
#[test]
fn duplicate_weak_symbols_follow_native_link_semantics() {
    let temp = TempDir::new("duplicate-weak");
    let (d1, d2, d3) = (
        temp.path().join("d1"),
        temp.path().join("d2"),
        temp.path().join("d3"),
    );
    for d in [&d1, &d2, &d3] {
        std::fs::create_dir_all(d).unwrap();
    }
    let weak_a = make_archive(
        &d1,
        "__attribute__((weak)) unsigned long mirvm_dup_weak(void) { return 1UL; }\n",
    );
    let weak_b = make_archive(
        &d2,
        "__attribute__((weak)) unsigned long mirvm_dup_weak(void) { return 2UL; }\n",
    );
    let strong_c = make_archive(&d3, "unsigned long mirvm_dup_weak(void) { return 3UL; }\n");
    let cache = temp.path().join("cache");
    let mat = |a: &PathBuf| {
        materialize_for_target_in(
            a,
            &cache,
            "x86_64-unknown-linux-gnu",
            Path::new("cc"),
            &[],
            None,
        )
        .unwrap()
    };
    let (sa, sb, sc) = (mat(&weak_a), mat(&weak_b), mat(&strong_c));
    // All-weak: allowed (native first-wins isomorphic)
    reject_symbol_ambiguity(&[sa.clone(), sb.clone()])
        .expect("duplicate all-weak names must be allowed");
    // strong + weak: allowed (native strong-wins same resolution)
    reject_symbol_ambiguity(&[sa.clone(), sc.clone()]).expect("strong+weak must be allowed");
    // Two strong: remain rejected (native would already be a link error)
    let strong_d_dir = temp.path().join("d4");
    std::fs::create_dir_all(&strong_d_dir).unwrap();
    let strong_d = make_archive(
        &strong_d_dir,
        "unsigned long mirvm_dup_weak(void) { return 4UL; }\n",
    );
    let sd = mat(&strong_d);
    reject_symbol_ambiguity(&[sc, sd]).unwrap_err();
}

#[test]
fn symbol_already_in_process_is_accepted_under_handle_first_resolution() {
    // New semantics after dynsym archive-handle priority: collisions between archive-exported symbols
    // and existing process definitions (here deliberately using malloc, which every process defines)
    // are no longer rejected—the archive handle always resolves first, reproducible as native link-time binding
    // (the guest's own objects always beat host libraries of the same name).
    let temp = TempDir::new("process-symbol");
    let archive = make_archive(
        temp.path(),
        "void *malloc(unsigned long size) { (void)size; return (void *)0; }\n",
    );
    let shared_object = materialize_in(&archive, &temp.path().join("cache")).unwrap();

    reject_symbol_ambiguity(&[shared_object]).unwrap();
}
