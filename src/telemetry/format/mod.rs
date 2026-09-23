//! v0 binary format shared by the capture writer and offline tools.
//!
//! Wire bytes are always encoded explicitly as little endian. Rust struct layout is
//! deliberately not part of the file contract.
//!
//! The format is one hierarchy and the modules follow it: [`container`] is the file and the chunk
//! framing around each sealed group of pages, [`record`] is a record's envelope plus the records a
//! producer and a session write about themselves, and [`syscall`] is the one record about the
//! guest. What all of them share lives here: the version, magic, size, kind and flag constants, the
//! little-endian field codec, and the [`WireError`] that names the field a record got wrong.

pub(crate) mod container;
pub(crate) mod record;
pub(crate) mod syscall;

pub(crate) use container::{ChunkFooter, ChunkHeader, FileHeader};
pub(crate) use record::{Control, EngineContext, PageHeader, ProducerEnd, SessionEnd};
pub(crate) use syscall::{SyscallEnter, SyscallExit, SyscallSemantics};

pub(crate) const FORMAT_MAJOR: u16 = 0;
pub(crate) const FORMAT_MINOR: u16 = 0;
pub(crate) const SCHEMA_MAJOR: u16 = 0;
pub(crate) const SCHEMA_MINOR: u16 = 0;

pub(crate) const FILE_MAGIC: &[u8; 8] = b"MIRVLOG\0";
pub(crate) const CHUNK_MAGIC: &[u8; 8] = b"MVCHNK\0\0";
pub(crate) const COMMIT_MAGIC: &[u8; 8] = b"MVCMIT\0\0";

pub(crate) const FILE_HEADER_BYTES: usize = 160;
pub(crate) const CHUNK_HEADER_BYTES: usize = 64;
pub(crate) const CHUNK_FOOTER_BYTES: usize = 64;
pub(crate) const PAGE_HEADER_BYTES: usize = 64;
pub(crate) const SYSCALL_ENTER_BYTES: usize = 64;
pub(crate) const SYSCALL_EXIT_BYTES: usize = 24;
pub(crate) const ENGINE_CONTEXT_BYTES: usize = 16;
pub(crate) const PRODUCER_END_BYTES: usize = 128;
pub(crate) const SESSION_END_BYTES: usize = 128;
pub(crate) const SYSCALL_PAIR_BYTES: usize = SYSCALL_ENTER_BYTES + SYSCALL_EXIT_BYTES;

pub(crate) const PAGE_BYTES_4K: u32 = 4 * 1024;
pub(crate) const PAGE_BYTES_16K: u32 = 16 * 1024;
pub(crate) const PAGE_BYTES_64K: u32 = 64 * 1024;
pub(crate) const MAX_CHUNK_PAYLOAD_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) const CLOCK_NONE: u8 = 0;

pub(crate) const KIND_PAGE: u16 = 0x0001;
pub(crate) const KIND_PRODUCER_END: u16 = 0x0002;
pub(crate) const KIND_SESSION_END: u16 = 0x0003;
pub(crate) const KIND_SYSCALL_ENTER: u16 = 0x0101;
pub(crate) const KIND_SYSCALL_EXIT: u16 = 0x0102;
pub(crate) const KIND_ENGINE_CONTEXT: u16 = 0x0201;

pub(crate) const FLAG_CONTEXT_CONTROL: u8 = 1 << 0;
pub(crate) const FLAG_RAW: u8 = 1 << 1;
pub(crate) const FLAG_LIBC: u8 = 1 << 2;
const KNOWN_CONTROL_FLAGS: u8 = FLAG_CONTEXT_CONTROL | FLAG_RAW | FLAG_LIBC;

pub(crate) const STATUS_ERRNO_VALID: u64 = 1 << 32;
const STATUS_ERRNO_MASK: u64 = u32::MAX as u64;
const STATUS_KNOWN_MASK: u64 = STATUS_ERRNO_MASK | STATUS_ERRNO_VALID;

/// A record that does not match the v0 wire contract. The message names the field and the reason.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct WireError(String);

impl WireError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

fn require_exact_len(bytes: &[u8], expected: usize, what: &str) -> Result<(), WireError> {
    if bytes.len() != expected {
        return Err(WireError::new(format!(
            "{what} length is {}, expected {expected}",
            bytes.len()
        )));
    }
    Ok(())
}

fn require_magic(bytes: &[u8], magic: &[u8; 8], what: &str) -> Result<(), WireError> {
    if bytes.get(..8) != Some(magic) {
        return Err(WireError::new(format!("{what} magic mismatch")));
    }
    Ok(())
}

fn require_version(bytes: &[u8], offset: usize, what: &str) -> Result<(), WireError> {
    let major = read_u16(bytes, offset, "format major")?;
    let minor = read_u16(bytes, offset + 2, "format minor")?;
    if (major, minor) != (FORMAT_MAJOR, FORMAT_MINOR) {
        return Err(WireError::new(format!(
            "unsupported {what} format version {major}.{minor}"
        )));
    }
    Ok(())
}

fn require_eq_u16(bytes: &[u8], offset: usize, expected: u16, what: &str) -> Result<(), WireError> {
    let value = read_u16(bytes, offset, what)?;
    if value != expected {
        return Err(WireError::new(format!(
            "{what} is {value}, expected {expected}"
        )));
    }
    Ok(())
}

fn require_eq_u32(bytes: &[u8], offset: usize, expected: u32, what: &str) -> Result<(), WireError> {
    let value = read_u32(bytes, offset, what)?;
    if value != expected {
        return Err(WireError::new(format!(
            "{what} is {value}, expected {expected}"
        )));
    }
    Ok(())
}

fn require_zero(bytes: &[u8], range: std::ops::Range<usize>, what: &str) -> Result<(), WireError> {
    let value = bytes
        .get(range)
        .ok_or_else(|| WireError::new(format!("truncated while reading {what}")))?;
    if value.iter().any(|byte| *byte != 0) {
        return Err(WireError::new(format!("{what} must be zero in v0")));
    }
    Ok(())
}

fn read_array<const N: usize>(
    bytes: &[u8],
    offset: usize,
    what: &str,
) -> Result<[u8; N], WireError> {
    bytes
        .get(offset..offset + N)
        .ok_or_else(|| WireError::new(format!("truncated while reading {what}")))?
        .try_into()
        .map_err(|_| WireError::new(format!("invalid width while reading {what}")))
}

fn read_u16(bytes: &[u8], offset: usize, what: &str) -> Result<u16, WireError> {
    Ok(u16::from_le_bytes(read_array(bytes, offset, what)?))
}

fn read_u32(bytes: &[u8], offset: usize, what: &str) -> Result<u32, WireError> {
    Ok(u32::from_le_bytes(read_array(bytes, offset, what)?))
}

fn read_i32(bytes: &[u8], offset: usize, what: &str) -> Result<i32, WireError> {
    Ok(i32::from_le_bytes(read_array(bytes, offset, what)?))
}

fn read_u64(bytes: &[u8], offset: usize, what: &str) -> Result<u64, WireError> {
    Ok(u64::from_le_bytes(read_array(bytes, offset, what)?))
}

fn read_i64(bytes: &[u8], offset: usize, what: &str) -> Result<i64, WireError> {
    Ok(i64::from_le_bytes(read_array(bytes, offset, what)?))
}

fn put<const N: usize>(out: &mut [u8], offset: usize, value: &[u8; N]) {
    out[offset..offset + N].copy_from_slice(value);
}

fn put_u16(out: &mut [u8], offset: usize, value: u16) {
    put(out, offset, &value.to_le_bytes());
}

fn put_u32(out: &mut [u8], offset: usize, value: u32) {
    put(out, offset, &value.to_le_bytes());
}

fn put_i32(out: &mut [u8], offset: usize, value: i32) {
    put(out, offset, &value.to_le_bytes());
}

fn put_u64(out: &mut [u8], offset: usize, value: u64) {
    put(out, offset, &value.to_le_bytes());
}

fn put_i64(out: &mut [u8], offset: usize, value: i64) {
    put(out, offset, &value.to_le_bytes());
}

#[cfg(test)]
mod tests;
