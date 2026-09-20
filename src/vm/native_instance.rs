//! Per-Engine native images and their explicit constructor/destructor lifecycle.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};

use super::ir::{Module, native_entry_slot_name};

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

/// Constructor and destructor addresses after relocation. Images deliberately
/// remain mapped for the process, but their language lifecycle still runs once
/// per Engine instance.
#[derive(Debug, Default)]
pub(crate) struct NativeLifecycle {
    initializers: Box<[usize]>,
    finalizers: Box<[usize]>,
    state: AtomicU8,
}

const LIFECYCLE_UNSTARTED: u8 = 0;
const LIFECYCLE_STARTING: u8 = 1;
const LIFECYCLE_COMPLETED: u8 = 2;
const LIFECYCLE_FINALIZED: u8 = 3;

pub(crate) struct InitializerArgs {
    _argv_storage: Vec<CString>,
    _env_storage: Vec<CString>,
    argv: Vec<*mut libc::c_char>,
    envp: Vec<*mut libc::c_char>,
}

impl InitializerArgs {
    fn capture() -> Result<Self, String> {
        let argv_storage = std::env::args_os()
            .map(|value| {
                CString::new(value.as_os_str().as_bytes())
                    .map_err(|_| "process argument contains NUL".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let env_storage = std::env::vars_os()
            .map(|(key, value)| {
                let mut bytes = key.as_os_str().as_bytes().to_vec();
                bytes.push(b'=');
                bytes.extend_from_slice(value.as_os_str().as_bytes());
                CString::new(bytes).map_err(|_| "process environment contains NUL".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut argv = argv_storage
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        let mut envp = env_storage
            .iter()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        argv.push(std::ptr::null_mut());
        envp.push(std::ptr::null_mut());
        Ok(Self {
            _argv_storage: argv_storage,
            _env_storage: env_storage,
            argv,
            envp,
        })
    }
}

impl NativeLifecycle {
    pub(crate) fn new(initializers: Vec<usize>, finalizers: Vec<usize>) -> Self {
        Self {
            initializers: initializers.into_boxed_slice(),
            finalizers: finalizers.into_boxed_slice(),
            state: AtomicU8::new(LIFECYCLE_UNSTARTED),
        }
    }

    pub(crate) fn run_initializers(&self, args: &mut InitializerArgs) {
        if self
            .state
            .compare_exchange(
                LIFECYCLE_UNSTARTED,
                LIFECYCLE_STARTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        for &address in &self.initializers {
            let init: unsafe extern "C-unwind" fn(
                libc::c_int,
                *mut *mut libc::c_char,
                *mut *mut libc::c_char,
            ) = unsafe { std::mem::transmute(address) };
            unsafe {
                init(
                    (args.argv.len() - 1) as libc::c_int,
                    args.argv.as_mut_ptr(),
                    args.envp.as_mut_ptr(),
                )
            };
        }
        self.state.store(LIFECYCLE_COMPLETED, Ordering::Release);
    }

    pub(crate) fn run_finalizers(&self) {
        if self
            .state
            .compare_exchange(
                LIFECYCLE_COMPLETED,
                LIFECYCLE_FINALIZED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return;
        }
        for &address in &self.finalizers {
            let fini: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(address) };
            unsafe { fini() };
        }
    }
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
    let dir = crate::options::get().home.join("runtime-native");
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
    let dir = crate::options::get().home.join("lower-native");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("fail to create lower native directory: {e}"))?;
    let private = copy_unique(path, &dir, "lower")?;
    open_deferred(&private, true)
}

/// Map and relocate every per-Engine object while constructors are suppressed.
/// This must run after P1 closures exist and before slots/GOT are patched.
pub(crate) fn prepare_required_libraries(module: &mut Module) -> Result<(), String> {
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
    module.native_images = images;
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
    let hidden_symbols = crate::elfsym::hidden_symtab_values(&path.to_string_lossy())?;
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

#[derive(Default)]
struct DynamicLifecycle {
    init: Option<u64>,
    init_array: Option<(u64, u64)>,
    fini: Option<u64>,
    fini_array: Option<(u64, u64)>,
    loads: Vec<(u64, u64)>,
    executable_loads: Vec<(u64, u64)>,
}

impl DynamicLifecycle {
    fn executable_ranges(&self, bias: usize) -> Result<Box<[(usize, usize)]>, String> {
        self.executable_loads
            .iter()
            .map(|&(start, end)| {
                let start = add_bias(bias, start, "executable PT_LOAD")?;
                let end = add_bias(bias, end, "executable PT_LOAD")?;
                Ok((start, end))
            })
            .collect::<Result<Vec<_>, String>>()
            .map(Vec::into_boxed_slice)
    }

    fn materialize(self, bias: usize) -> Result<NativeLifecycle, String> {
        let mut initializers = Vec::new();
        if let Some(init) = self.init {
            initializers.push(add_bias(bias, init, "DT_INIT")?);
        }
        if let Some((address, size)) = self.init_array {
            initializers.extend(read_function_array(
                bias,
                address,
                size,
                &self.loads,
                "DT_INIT_ARRAY",
            )?);
        }

        let mut finalizers = Vec::new();
        if let Some((address, size)) = self.fini_array {
            let mut array = read_function_array(bias, address, size, &self.loads, "DT_FINI_ARRAY")?;
            array.reverse();
            finalizers.extend(array);
        }
        if let Some(fini) = self.fini {
            finalizers.push(add_bias(bias, fini, "DT_FINI")?);
        }
        Ok(NativeLifecycle::new(initializers, finalizers))
    }
}

fn add_bias(bias: usize, value: u64, what: &str) -> Result<usize, String> {
    bias.checked_add(
        usize::try_from(value).map_err(|_| format!("{what} address does not fit usize"))?,
    )
    .ok_or_else(|| format!("{what} address overflow"))
}

fn read_function_array(
    bias: usize,
    address: u64,
    size: u64,
    loads: &[(u64, u64)],
    what: &str,
) -> Result<Vec<usize>, String> {
    if !size.is_multiple_of(8) || size > 1 << 20 {
        return Err(format!("{what} has invalid size {size}"));
    }
    let end = address
        .checked_add(size)
        .ok_or_else(|| format!("{what} range overflow"))?;
    if !loads
        .iter()
        .any(|&(start, load_end)| address >= start && end <= load_end)
    {
        return Err(format!("{what} lies outside a loadable segment"));
    }
    let start = add_bias(bias, address, what)?;
    let mut functions = Vec::with_capacity((size / 8) as usize);
    for index in 0..(size / 8) as usize {
        let value = unsafe { (start as *const usize).add(index).read_unaligned() };
        if value != 0 && value != usize::MAX {
            functions.push(value);
        }
    }
    Ok(functions)
}

/// Remove dynamic-loader ownership of init/fini from this unique private copy.
/// Mapping, dependency resolution and relocations still use dlopen; the saved
/// addresses are invoked explicitly only after the Engine patches P1 slots.
fn suppress_lifecycle_tags(path: &Path) -> Result<DynamicLifecycle, String> {
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const DT_NULL: i64 = 0;
    const DT_INIT: i64 = 12;
    const DT_FINI: i64 = 13;
    const DT_BIND_NOW: i64 = 24;
    const DT_INIT_ARRAY: i64 = 25;
    const DT_FINI_ARRAY: i64 = 26;
    const DT_INIT_ARRAYSZ: i64 = 27;
    const DT_FINI_ARRAYSZ: i64 = 28;
    const DT_PREINIT_ARRAY: i64 = 32;
    const DT_PREINIT_ARRAYSZ: i64 = 33;

    let mut bytes = std::fs::read(path)
        .map_err(|e| format!("fail to read private native `{}`: {e}", path.display()))?;
    let bad = || format!("native `{}` is not valid ELF64 LE", path.display());
    if bytes.len() < 64 || bytes[0..6] != [0x7f, b'E', b'L', b'F', 2, 1] {
        return Err(bad());
    }
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
    if u16_at(16) != Some(3) || u16_at(18) != Some(62) {
        return Err(bad());
    }
    let phoff = usize::try_from(u64_at(32).ok_or_else(bad)?).map_err(|_| bad())?;
    let phentsize = usize::from(u16_at(54).ok_or_else(bad)?);
    let phnum = usize::from(u16_at(56).ok_or_else(bad)?);
    if phentsize < 56 {
        return Err(bad());
    }
    let mut dynamic = None;
    let mut result = DynamicLifecycle::default();
    for index in 0..phnum {
        let base = phoff
            .checked_add(index.checked_mul(phentsize).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        let ty = u32_at(base).ok_or_else(bad)?;
        let flags = u32_at(base + 4).ok_or_else(bad)?;
        let off = u64_at(base + 8).ok_or_else(bad)?;
        let vaddr = u64_at(base + 16).ok_or_else(bad)?;
        let filesz = u64_at(base + 32).ok_or_else(bad)?;
        let memsz = u64_at(base + 40).ok_or_else(bad)?;
        if ty == PT_LOAD {
            let end = vaddr
                .checked_add(memsz)
                .ok_or_else(|| format!("native `{}` load range overflow", path.display()))?;
            result.loads.push((vaddr, end));
            if flags & 1 != 0 {
                result.executable_loads.push((vaddr, end));
            }
        } else if ty == PT_DYNAMIC {
            dynamic = Some((off, filesz));
        }
    }
    let Some((dynamic_off, dynamic_size)) = dynamic else {
        return Err(format!("native `{}` has no PT_DYNAMIC", path.display()));
    };
    let start = usize::try_from(dynamic_off).map_err(|_| bad())?;
    let size = usize::try_from(dynamic_size).map_err(|_| bad())?;
    let end = start.checked_add(size).ok_or_else(bad)?;
    if end > bytes.len() || size % 16 != 0 {
        return Err(bad());
    }

    let mut init_array_addr = None;
    let mut init_array_size = None;
    let mut fini_array_addr = None;
    let mut fini_array_size = None;
    for entry in (start..end).step_by(16) {
        let tag = i64::from_le_bytes(bytes[entry..entry + 8].try_into().map_err(|_| bad())?);
        if tag == DT_NULL {
            break;
        }
        let value = u64::from_le_bytes(bytes[entry + 8..entry + 16].try_into().map_err(|_| bad())?);
        match tag {
            DT_INIT => result.init = Some(value),
            DT_FINI => result.fini = Some(value),
            DT_INIT_ARRAY => init_array_addr = Some(value),
            DT_INIT_ARRAYSZ => init_array_size = Some(value),
            DT_FINI_ARRAY => fini_array_addr = Some(value),
            DT_FINI_ARRAYSZ => fini_array_size = Some(value),
            DT_PREINIT_ARRAY | DT_PREINIT_ARRAYSZ => {
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
        bytes[entry..entry + 8].copy_from_slice(&DT_BIND_NOW.to_le_bytes());
        bytes[entry + 8..entry + 16].fill(0);
    }
    result.init_array = pair_tags(init_array_addr, init_array_size, "DT_INIT_ARRAY")?;
    result.fini_array = pair_tags(fini_array_addr, fini_array_size, "DT_FINI_ARRAY")?;
    std::fs::write(path, bytes).map_err(|e| {
        format!(
            "fail to suppress native lifecycle `{}`: {e}",
            path.display()
        )
    })?;
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

/// Fill every native bridge slot after all per-Engine P1 closures exist.
pub(crate) fn patch_entry_slots(module: &Module) -> Result<(), String> {
    let mut slots = BTreeMap::<String, u64>::new();
    for site in module.entry_stub_sites.iter().chain(
        module
            .image_entry_stubs
            .iter()
            .flat_map(|(_, sites, _)| sites.iter()),
    ) {
        let target = module.try_resolve_link_addr(site.link_addr)?;
        slots.insert(native_entry_slot_name(site.link_addr), target);
    }
    if slots.is_empty() {
        return Ok(());
    }

    for image in &module.mc_images {
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
    if module.native_images.len() != module.required_native_libs.len() {
        return Err("native image/path count mismatch".into());
    }
    for image in &module.native_images {
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
pub(crate) fn patch_pthread_slots(module: &Module, engine_id: u64) -> Result<(), String> {
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
    for image in &module.native_images {
        patch(image.hidden_symbol_values(), image.bias())?;
    }
    for image in &module.mc_images {
        patch(&image.symbols, image.load_bias() as u64)?;
    }
    Ok(())
}

pub(crate) fn commit_images(module: &Module, control: &Arc<super::ctx::EngineControl>) {
    for image in &module.native_images {
        image.committed.store(true, Ordering::Release);
    }
    for image in &module.mc_images {
        image.commit();
    }
    let mut owned = OWNED_NATIVE_CODE.write().unwrap();
    for &(start, end) in module
        .native_images
        .iter()
        .flat_map(NativeImage::executable_ranges)
        .chain(
            module
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

pub(crate) fn run_initializers(module: &Module) -> Result<(), String> {
    let mut args = InitializerArgs::capture()?;
    for image in &module.native_images {
        image.lifecycle.run_initializers(&mut args);
    }
    for image in &module.mc_images {
        image.run_initializers(&mut args);
    }
    Ok(())
}

pub(crate) fn run_finalizers(module: &Module) {
    for image in module.mc_images.iter().rev() {
        image.run_finalizers();
    }
    for image in module.native_images.iter().rev() {
        image.lifecycle.run_finalizers();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{InitializerArgs, NativeLifecycle};

    static GOOD_INIT: AtomicU64 = AtomicU64::new(0);
    static BAD_INIT: AtomicU64 = AtomicU64::new(0);
    static GOOD_FINI: AtomicU64 = AtomicU64::new(0);
    static BAD_FINI: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C-unwind" fn good_init(
        argc: libc::c_int,
        argv: *mut *mut libc::c_char,
        envp: *mut *mut libc::c_char,
    ) {
        assert!(argc > 0);
        assert!(!argv.is_null());
        assert!(!envp.is_null());
        GOOD_INIT.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C-unwind" fn bad_init(
        _: libc::c_int,
        _: *mut *mut libc::c_char,
        _: *mut *mut libc::c_char,
    ) {
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
