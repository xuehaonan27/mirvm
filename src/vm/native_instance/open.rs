//! Opening a per-Engine object through the system loader with its constructors suppressed.
//!
//! Mapping, dependency resolution and relocation are `dlopen`'s; init/fini timing is not. The
//! private copy is read for its dynamic tags first and those tags are rewritten, so the system
//! loader performs no constructor call and `native_lifecycle` can run them at the point the
//! Engine has patched the bridge slots. The self-mapping path is [`super::super::mcload`].

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use super::super::instance::Instance;
use super::super::ir::Module;
use super::super::native_lifecycle::suppress_lifecycle_tags;
use super::NativeImage;
use super::identity::copy_unique;

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
    let bias = crate::os::dll::load_bias(handle, &cpath)
        .ok_or_else(|| format!("fail to find load base for `{}`", path.display()))?;
    let hidden_symbols = crate::native::symtab::hidden_symtab_values(
        &path.to_string_lossy(),
        crate::os::dll::OBJECT_FORMAT,
    )
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
