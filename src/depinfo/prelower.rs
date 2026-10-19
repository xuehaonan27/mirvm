//! Completion receipts for dependency bytecode preparation beside compiler artifacts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::utils::content::FileStamp;

#[derive(Serialize, Deserialize)]
struct Receipt {
    build_id: String,
    artifact: FileStamp,
    products: Vec<FileStamp>,
}

fn path(artifact: &Path) -> PathBuf {
    artifact.with_extension("mirvm-prelower")
}

pub(crate) fn is_current(artifact: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path(artifact)) else {
        return false;
    };
    let Ok(receipt) = postcard::from_bytes::<Receipt>(&bytes) else {
        return false;
    };
    crate::store::entry::is_current_generation(&receipt.build_id)
        && receipt.artifact.path == artifact.to_string_lossy()
        && receipt.artifact.is_current()
        && receipt.products.iter().all(FileStamp::is_current)
}

#[cfg(feature = "compiler")]
pub(crate) fn publish(
    artifact: &Path,
    products: Vec<FileStamp>,
) -> Result<(), crate::error::Error> {
    let artifact_stamp = FileStamp::of(artifact).map_err(|error| {
        crate::fail!(Build, format!("cannot stamp dependency artifact: {error}"))
    })?;
    let receipt = Receipt {
        build_id: crate::options::build::BUILD_ID.into(),
        artifact: artifact_stamp,
        products,
    };
    let bytes = postcard::to_stdvec(&receipt)
        .map_err(|error| crate::fail!(Build, format!("cannot encode prelower receipt: {error}")))?;
    crate::store::publish_bytes(&path(artifact), &bytes)
        .map_err(|error| crate::fail!(Build, format!("cannot publish prelower receipt: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_requires_unchanged_artifact_and_products() {
        let dir = std::env::temp_dir().join(format!("mirvm-prelower-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let artifact = dir.join("libdep.rlib");
        let product = dir.join("templates.bin");
        std::fs::write(&artifact, b"metadata").unwrap();
        std::fs::write(&product, b"bytecode").unwrap();
        let artifact_time = std::fs::metadata(&artifact).unwrap().modified().unwrap();
        let product_time = std::fs::metadata(&product).unwrap().modified().unwrap();
        let restore_time = |path: &Path, time| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        };
        let receipt = Receipt {
            build_id: crate::options::build::BUILD_ID.into(),
            artifact: FileStamp::of(&artifact).unwrap(),
            products: vec![FileStamp::of(&product).unwrap()],
        };
        std::fs::write(path(&artifact), postcard::to_stdvec(&receipt).unwrap()).unwrap();
        assert!(is_current(&artifact));
        std::fs::write(&product, b"damaged!").unwrap();
        restore_time(&product, product_time);
        assert!(!is_current(&artifact));
        std::fs::write(&product, b"bytecode").unwrap();
        restore_time(&product, product_time);
        assert!(is_current(&artifact));
        std::fs::write(&artifact, b"replaced").unwrap();
        assert!(!is_current(&artifact));
        std::fs::write(&artifact, b"metadata").unwrap();
        restore_time(&artifact, artifact_time);
        assert!(is_current(&artifact));
        std::fs::remove_file(&product).unwrap();
        assert!(!is_current(&artifact));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
