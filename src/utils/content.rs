use std::io::{self, ErrorKind};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::os;

/// Identity of one input file: the path it was read from, its cheap metadata for diagnostics, and
/// the BLAKE3 digest that carries the content identity — size and mtime alone would miss a
/// same-length rewrite with a restored mtime.
///
/// This is the recorded form (cache and package headers serialize it), so a later load replays it
/// against the current file state with [`FileStamp::is_current`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub path: String,
    pub size: u64,
    pub mtime_ns: u128,
    pub digest: [u8; 32],
}

impl FileStamp {
    /// Read a regular file through one descriptor and bind its cheap metadata to a content digest. A
    /// file changing while it is read is rejected, so cache writers and readers fail closed instead
    /// of recording a mixed snapshot.
    pub(crate) fn of(path: &Path) -> io::Result<FileStamp> {
        let mut file = std::fs::File::open(path)?;
        let before = file.metadata()?;
        if !before.is_file() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(&mut file)?;
        let after = file.metadata()?;
        let path_after = std::fs::metadata(path)?;
        let after_mtime = mtime_ns(&after)?;
        if !os::fs::same_file(&before, &after) || !os::fs::same_file(&after, &path_after) {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "file changed while hashing",
            ));
        }
        Ok(FileStamp {
            path: path.display().to_string(),
            size: after.len(),
            mtime_ns: after_mtime,
            digest: *hasher.finalize().as_bytes(),
        })
    }

    /// Whether `path` still holds the stamped content.
    pub(crate) fn is_current(&self) -> bool {
        FileStamp::of(Path::new(&self.path)).is_ok_and(|now| now == *self)
    }
}

/// FNV-1a 64-bit content hash.
///
/// Cache keys, cargoless unit fingerprints and the asm-stub/global-asm factory keys all use it, so
/// it lives in the leaf utility module rather than in the lowering engine it was first written for.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn mtime_ns(metadata: &std::fs::Metadata) -> io::Result<u128> {
    metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))
}

pub(crate) fn digest_hex(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mirvm-content-test-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("input.rs")
    }

    #[test]
    fn stamp_detects_length_mtime_and_missing_file() {
        let file = temp_file("stamp");
        std::fs::write(&file, b"fn main() {}").unwrap();
        let first = FileStamp::of(&file).expect("stampable");

        // size change must mismatch
        std::fs::write(&file, b"fn main() { let _ = 1; }").unwrap();
        assert!(!first.is_current());

        // same size but later mtime must also mismatch (equal-length rewrites are caught by mtime)
        std::fs::write(&file, b"fn main() {}").unwrap();
        let second = FileStamp::of(&file).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(7);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(!second.is_current());

        // a missing file is not current, and cannot be stamped
        let third = FileStamp::of(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert!(FileStamp::of(&file).is_err());
        assert!(!third.is_current());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn stamp_detects_same_length_content_change_with_restored_mtime() {
        let file = temp_file("content");
        std::fs::write(&file, b"fn value() -> u8 { 1 }").unwrap();
        let original_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        let before = FileStamp::of(&file).expect("stampable");

        std::fs::write(&file, b"fn value() -> u8 { 2 }").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(original_mtime)
            .unwrap();
        assert!(!before.is_current());

        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}
