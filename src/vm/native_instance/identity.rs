//! Give each Engine its own native-library file identity.
//!
//! glibc keys loaded objects by file identity/path, so reusing the content-addressed cache path
//! would merge mutable globals between Engines. Each Engine therefore loads a private copy of
//! every required library, verified against the hash the package recorded before it is used.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::super::ir::Module;

static NEXT_NATIVE_INSTANCE: AtomicU64 = AtomicU64::new(0);

/// Rewrite `module.required_native_libs` to this Engine's private copies, so the Engine's
/// `dlopen` cannot share an already-loaded object with another Engine.
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

/// Copy `source` into `dir` under a name unique to this process and owner, so a concurrent
/// Engine or a repeated load never reuses a path glibc already keyed an object by.
pub(super) fn copy_unique(source: &Path, dir: &Path, owner: &str) -> Result<PathBuf, String> {
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
