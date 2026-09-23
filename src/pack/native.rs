//! Putting a package's native libraries where the loader looks for them: into the store, keyed by
//! content, so two packages that share a library share one file.

use std::path::{Path, PathBuf};

use super::Error;
use super::format::hash128;
use super::meta::{NativeLibEntry, check_lib_hash};

pub(super) fn materialize_native_blob_at(
    dir: &Path,
    lib: &NativeLibEntry,
) -> Result<PathBuf, Error> {
    check_lib_hash(lib)?;
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::io("cannot create the package native directory", e))?;
    let path = dir.join(format!("{:032x}.so", lib.fnv));
    // Already published under this digest: republishing would only rewrite the same bytes.
    if std::fs::read(&path)
        .ok()
        .is_some_and(|bytes| hash128(&bytes) == lib.fnv)
    {
        return Ok(path);
    }
    crate::store::publish_bytes(&path, &lib.bytes)
        .map_err(|e| Error::io("cannot publish the package native library", e))?;
    Ok(path)
}
