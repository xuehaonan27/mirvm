use std::io::{self, ErrorKind};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileContentStamp {
    pub size: u64,
    pub mtime_ns: u128,
    pub digest: [u8; 32],
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

#[cfg(unix)]
fn same_file_snapshot(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_file_snapshot(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len() && left.modified().ok() == right.modified().ok()
}

/// Read a regular file through one descriptor and bind its cheap metadata to
/// a content digest. A file changing while it is read is rejected, so cache
/// writers and readers fail closed instead of recording a mixed snapshot.
pub(crate) fn file_content_stamp(path: &Path) -> io::Result<FileContentStamp> {
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
    if !same_file_snapshot(&before, &after) || !same_file_snapshot(&after, &path_after) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "file changed while hashing",
        ));
    }
    Ok(FileContentStamp {
        size: after.len(),
        mtime_ns: after_mtime,
        digest: *hasher.finalize().as_bytes(),
    })
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
