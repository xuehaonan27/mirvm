//! Per-Engine native images and their explicit constructor/destructor lifecycle.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

use super::instance::Instance;
use super::ir::{Module, native_entry_slot_name};
use super::native_lifecycle::{InitializerArgs, NativeLifecycle, suppress_lifecycle_tags};

static NEXT_NATIVE_INSTANCE: AtomicU64 = AtomicU64::new(0);

/// A mapped self-produced shared object. The system loader performs dependency
/// resolution and ELF relocation, but mirvm controls init/fini timing so P1
/// callback slots are valid before any constructor can call guest code.
#[derive(Debug)]
#[doc(hidden)]
pub struct NativeImage {
    handle: usize,
    bias: u64,
    hidden_symbols: HashMap<Box<str>, u64>,
    executable_ranges: Box<[(usize, usize)]>,
    lifecycle: NativeLifecycle,
    committed: AtomicBool,
}

impl NativeImage {
    pub(crate) fn handle(&self) -> usize {
        self.handle
    }

    pub(crate) fn bias(&self) -> u64 {
        self.bias
    }

    pub(crate) fn hidden_symbol_values(&self) -> &HashMap<Box<str>, u64> {
        &self.hidden_symbols
    }

    fn executable_ranges(&self) -> &[(usize, usize)] {
        &self.executable_ranges
    }
}

struct OwnedNativeCode {
    start: usize,
    end: usize,
    control: Arc<super::ctx::EngineControl>,
}

/// Process-lifetime ownership for executable code emitted from guest inputs.
/// Committed images are intentionally never unmapped, so an address returned
/// through `oldact` remains attributable even after its Engine has closed.
static OWNED_NATIVE_CODE: LazyLock<RwLock<Vec<OwnedNativeCode>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

pub(crate) fn owner_of_executable_address(
    address: usize,
) -> Option<Arc<super::ctx::EngineControl>> {
    OWNED_NATIVE_CODE
        .read()
        .unwrap()
        .iter()
        .rev()
        .find(|range| address >= range.start && address < range.end)
        .map(|range| Arc::clone(&range.control))
}

// Native function addresses and dlopen handles are immutable after publication.
unsafe impl Send for NativeImage {}
unsafe impl Sync for NativeImage {}

impl Drop for NativeImage {
    fn drop(&mut self) {
        if !self.committed.load(Ordering::Acquire) {
            unsafe { crate::os::dll::close(self.handle) };
        }
    }
}

/// Give this Engine its own native-library file identity. glibc keys loaded
/// objects by file identity/path, so reusing the content-addressed cache path
/// would merge mutable globals between Engines.
pub(crate) fn isolate_required_libraries(
    module: &mut Module,
    engine_id: u64,
) -> Result<(), String> {
    if module.required_native_libs.is_empty() {
        return Ok(());
    }
    let dir = crate::store::RUNTIME_NATIVE.dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("fail to create per-Engine native directory: {e}"))?;
    if !module.required_native_hashes.is_empty()
        && module.required_native_hashes.len() != module.required_native_libs.len()
    {
        return Err("required native library/hash count mismatch".into());
    }
    let mut isolated: Vec<PathBuf> = Vec::with_capacity(module.required_native_libs.len());
    for (index, path) in module.required_native_libs.iter().enumerate() {
        let private = match copy_unique(Path::new(&**path), &dir, &format!("engine-{engine_id}")) {
            Ok(private) => private,
            Err(error) => {
                for path in isolated {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error);
            }
        };
        if let Some(&expected) = module.required_native_hashes.get(index) {
            let bytes = match std::fs::read(&private) {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = std::fs::remove_file(&private);
                    for path in isolated {
                        let _ = std::fs::remove_file(path);
                    }
                    return Err(format!(
                        "fail to verify private native library `{}`: {error}",
                        private.display()
                    ));
                }
            };
            if package_native_hash(&bytes) != expected {
                let _ = std::fs::remove_file(&private);
                for path in isolated {
                    let _ = std::fs::remove_file(path);
                }
                return Err(format!(
                    "required native library `{}` changed after package verification",
                    path
                ));
            }
        }
        isolated.push(private);
    }
    module.required_native_libs = isolated
        .iter()
        .map(|path| path.to_string_lossy().into_owned().into_boxed_str())
        .collect();
    Ok(())
}

fn package_native_hash(data: &[u8]) -> u128 {
    let a = crate::utils::content::fnv1a(data);
    let mut b = 0xcbf2_9ce4_8422_2325u64;
    for byte in b"\x01mirvmar".iter().chain(data) {
        b ^= u64::from(*byte);
        b = b.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ((a as u128) << 64) | u128::from(b)
}

fn copy_unique(source: &Path, dir: &Path, owner: &str) -> Result<PathBuf, String> {
    let serial = NEXT_NATIVE_INSTANCE.fetch_add(1, Ordering::Relaxed);
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("native.so");
    let private = dir.join(format!("{}-{owner}-{serial}-{name}", std::process::id()));
    std::fs::copy(source, &private).map_err(|e| {
        let _ = std::fs::remove_file(&private);
        format!(
            "fail to copy native library `{}` for {owner}: {e}",
            source.display()
        )
    })?;
    Ok(private)
}

/// Lowering needs symbols from self-produced objects, but native constructors
/// belong to Engine startup, not compilation. Load a private copy with its
/// lifecycle deferred and intentionally keep that mapping for the process.
pub(crate) fn open_for_lower(path: &Path) -> Result<NativeImage, String> {
    let dir = crate::store::LOWER_NATIVE.dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("fail to create lower native directory: {e}"))?;
    let private = copy_unique(path, &dir, "lower")?;
    open_deferred(&private, true)
}

/// Map and relocate every per-Engine object while constructors are suppressed.
/// This must run after P1 closures exist and before slots/GOT are patched.
pub(crate) fn prepare_required_libraries(
    module: &Module,
    instance: &mut Instance,
) -> Result<(), String> {
    let mut images = Vec::with_capacity(module.required_native_libs.len());
    for path in &module.required_native_libs {
        match open_deferred(Path::new(&**path), true) {
            Ok(image) => images.push(image),
            Err(error) => {
                for private in &module.required_native_libs {
                    let _ = std::fs::remove_file(Path::new(&**private));
                }
                return Err(error);
            }
        }
    }
    instance.native_images = images;
    Ok(())
}

fn open_deferred(path: &Path, remove_private_file: bool) -> Result<NativeImage, String> {
    struct FileGuard(Option<PathBuf>);
    impl Drop for FileGuard {
        fn drop(&mut self) {
            if let Some(path) = self.0.take() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let file_guard = FileGuard(remove_private_file.then(|| path.to_path_buf()));
    let tags = suppress_lifecycle_tags(path)?;
    let cpath = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| format!("per-Engine native path contains NUL: {}", path.display()))?;
    let handle = crate::os::dll::open_with_flags(
        &cpath,
        crate::os::dll::RTLD_NOW | crate::os::dll::RTLD_LOCAL,
    )
    .map_err(|e| format!("fail to map/relocate native `{}`: {e}", path.display()))?;
    struct HandleGuard(Option<usize>);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                unsafe { crate::os::dll::close(handle) };
            }
        }
    }
    let mut handle_guard = HandleGuard(Some(handle));
    crate::lower::asm::refill_syscall_slot(handle);
    let bias = crate::os::dll::load_bias(handle)
        .ok_or_else(|| format!("fail to find load base for `{}`", path.display()))?;
    let hidden_symbols = crate::native::symtab::hidden_symtab_values(&path.to_string_lossy())
        .map_err(|error| error.to_string())?;
    let executable_ranges = tags.executable_ranges(bias)?;
    let lifecycle = tags.materialize(bias)?;
    handle_guard.0 = None;
    // The mapped object and its handle no longer need the directory entry.
    // Unlink before publishing the image so both successful and failed
    // instances have bounded temporary-file lifetime.
    drop(file_guard);
    Ok(NativeImage {
        handle,
        bias: bias as u64,
        hidden_symbols,
        executable_ranges,
        lifecycle,
        committed: AtomicBool::new(false),
    })
}

/// Fill every native bridge slot after all per-Engine P1 closures exist.
pub(crate) fn patch_entry_slots(module: &Module, instance: &Instance) -> Result<(), String> {
    let mut slots = BTreeMap::<String, u64>::new();
    for site in module.entry_stub_sites.iter().chain(
        instance
            .image_entry_stubs
            .iter()
            .flat_map(|(_, sites, _)| sites.iter()),
    ) {
        let target = instance.try_resolve_link_addr(site.link_addr)?;
        slots.insert(native_entry_slot_name(site.link_addr), target);
    }
    if slots.is_empty() {
        return Ok(());
    }

    for image in &instance.mc_images {
        for (name, &value) in image
            .symbols
            .iter()
            .filter(|(name, _)| name.starts_with("__mirvm_p1_target_"))
        {
            let target = slots
                .get(name.as_ref())
                .ok_or_else(|| format!("native entry slot `{name}` has no P1 recipe"))?;
            let slot = (image.load_bias() as u64)
                .checked_add(value)
                .ok_or_else(|| format!("native entry slot `{name}` address overflow"))?;
            unsafe { (slot as *mut u64).write(*target) };
        }
    }
    if instance.native_images.len() != module.required_native_libs.len() {
        return Err("native image/path count mismatch".into());
    }
    for image in &instance.native_images {
        for (name, &value) in image
            .hidden_symbol_values()
            .iter()
            .filter(|(name, _)| name.starts_with("__mirvm_p1_target_"))
        {
            let target = slots
                .get(name.as_ref())
                .ok_or_else(|| format!("native entry slot `{name}` has no P1 recipe"))?;
            let slot = image
                .bias
                .checked_add(value)
                .ok_or_else(|| format!("native entry slot `{name}` address overflow"))?;
            unsafe { (slot as *mut u64).write(*target) };
        }
    }
    Ok(())
}

/// Fill the private runtime interposition slots injected into every
/// self-produced machine-code image. Images are already relocated but no
/// constructor has run yet.
pub(crate) fn patch_pthread_slots(instance: &Instance, engine_id: u64) -> Result<(), String> {
    let targets = [
        (
            "__mirvm_pthread_create_target",
            super::deferred::native_pthread_create as *const () as usize as u64,
        ),
        (
            "__mirvm_pthread_key_create_target",
            super::deferred::native_pthread_key_create as *const () as usize as u64,
        ),
        (
            "__mirvm_pthread_setspecific_target",
            super::deferred::native_pthread_setspecific as *const () as usize as u64,
        ),
        (
            "__mirvm_pthread_key_delete_target",
            super::deferred::native_pthread_key_delete as *const () as usize as u64,
        ),
        (
            "__mirvm_signal_target",
            super::signal::native_signal as *const () as usize as u64,
        ),
        (
            "__mirvm_sigaction_target",
            super::signal::native_sigaction as *const () as usize as u64,
        ),
        (
            "__mirvm_raise_target",
            super::signal::native_raise as *const () as usize as u64,
        ),
    ];
    const OWNERS: [&str; 2] = ["__mirvm_pthread_owner", "__mirvm_signal_owner"];
    let patch = |symbols: &HashMap<Box<str>, u64>, bias: u64| -> Result<(), String> {
        let present = targets
            .iter()
            .filter(|(name, _)| symbols.contains_key(*name))
            .count()
            + OWNERS
                .iter()
                .filter(|name| symbols.contains_key(**name))
                .count();
        if present == 0 {
            return Ok(());
        }
        if present != targets.len() + OWNERS.len() {
            return Err("self-produced native image has an incomplete runtime bridge".into());
        }
        for &(name, target) in &targets {
            let value = symbols.get(name).ok_or_else(|| {
                format!("self-produced native image has no runtime bridge slot `{name}`")
            })?;
            let slot = bias
                .checked_add(*value)
                .ok_or_else(|| format!("runtime bridge slot `{name}` address overflow"))?;
            unsafe { (slot as *mut u64).write(target) };
        }
        for name in OWNERS {
            let owner = symbols.get(name).ok_or_else(|| {
                format!("self-produced native image has no runtime owner slot `{name}`")
            })?;
            let slot = bias
                .checked_add(*owner)
                .ok_or_else(|| format!("runtime owner slot `{name}` address overflow"))?;
            unsafe { (slot as *mut u64).write(engine_id) };
        }
        Ok(())
    };
    for image in &instance.native_images {
        patch(image.hidden_symbol_values(), image.bias())?;
    }
    for image in &instance.mc_images {
        patch(&image.symbols, image.load_bias() as u64)?;
    }
    Ok(())
}

pub(crate) fn commit_images(instance: &Instance, control: &Arc<super::ctx::EngineControl>) {
    for image in &instance.native_images {
        image.committed.store(true, Ordering::Release);
    }
    for image in &instance.mc_images {
        image.commit();
    }
    let mut owned = OWNED_NATIVE_CODE.write().unwrap();
    for &(start, end) in instance
        .native_images
        .iter()
        .flat_map(NativeImage::executable_ranges)
        .chain(
            instance
                .mc_images
                .iter()
                .flat_map(super::mcload::McImage::executable_ranges),
        )
    {
        owned.push(OwnedNativeCode {
            start,
            end,
            control: Arc::clone(control),
        });
    }
}

pub(crate) fn run_initializers(instance: &Instance) -> Result<(), String> {
    let mut args = InitializerArgs::capture()?;
    for image in &instance.native_images {
        image.lifecycle.run_initializers(&mut args);
    }
    for image in &instance.mc_images {
        image.run_initializers(&mut args);
    }
    Ok(())
}

pub(crate) fn run_finalizers(instance: &Instance) {
    for image in instance.mc_images.iter().rev() {
        image.run_finalizers();
    }
    for image in instance.native_images.iter().rev() {
        image.lifecycle.run_finalizers();
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{c_char, c_int};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::vm::native_lifecycle::{InitializerArgs, NativeLifecycle};

    static GOOD_INIT: AtomicU64 = AtomicU64::new(0);
    static BAD_INIT: AtomicU64 = AtomicU64::new(0);
    static GOOD_FINI: AtomicU64 = AtomicU64::new(0);
    static BAD_FINI: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C-unwind" fn good_init(
        argc: c_int,
        argv: *mut *mut c_char,
        envp: *mut *mut c_char,
    ) {
        assert!(argc > 0);
        assert!(!argv.is_null());
        assert!(!envp.is_null());
        GOOD_INIT.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C-unwind" fn bad_init(_: c_int, _: *mut *mut c_char, _: *mut *mut c_char) {
        BAD_INIT.fetch_add(1, Ordering::SeqCst);
        panic!("constructor failed after starting");
    }

    unsafe extern "C-unwind" fn good_fini() {
        GOOD_FINI.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C-unwind" fn bad_fini() {
        BAD_FINI.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn only_fully_initialized_images_are_finalized() {
        GOOD_INIT.store(0, Ordering::SeqCst);
        BAD_INIT.store(0, Ordering::SeqCst);
        GOOD_FINI.store(0, Ordering::SeqCst);
        BAD_FINI.store(0, Ordering::SeqCst);
        let good = NativeLifecycle::new(
            vec![good_init as *const () as usize],
            vec![good_fini as *const () as usize],
        );
        let bad = NativeLifecycle::new(
            vec![bad_init as *const () as usize],
            vec![bad_fini as *const () as usize],
        );
        let mut args = InitializerArgs::capture().unwrap();

        good.run_initializers(&mut args);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                bad.run_initializers(&mut args)
            }))
            .is_err()
        );
        bad.run_finalizers();
        good.run_finalizers();
        good.run_finalizers();

        assert_eq!(GOOD_INIT.load(Ordering::SeqCst), 1);
        assert_eq!(BAD_INIT.load(Ordering::SeqCst), 1);
        assert_eq!(GOOD_FINI.load(Ordering::SeqCst), 1);
        assert_eq!(BAD_FINI.load(Ordering::SeqCst), 0);
    }
}
