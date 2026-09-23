//! Little-endian field IO for the container and its section codecs.
//!
//! Two callers need these: the container's own header and section table, and the FUNCS section's
//! index. That is why they are a module rather than a private detail of either. A width that does
//! not fit where the layout puts it is a build failure when writing and corruption when reading, so
//! each direction raises its own class rather than sharing one.

use super::Error;

/// A reader over a byte slice that never runs off the end; a failure names what was being read.
pub(super) struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(super) fn new(data: &'a [u8], pos: usize) -> Self {
        Cursor { data, pos }
    }

    /// How far the reader has come: the offset of the next field, for a layout that has to ask
    /// rather than recompute the sizes it just read. The writer's twin is [`Writer::len`].
    pub(super) fn pos(&self) -> usize {
        self.pos
    }

    pub(super) fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| Error::corrupt(format!("package {what} length overflow")))?;
        let value = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| Error::corrupt(format!("package truncated while reading {what}")))?;
        self.pos = end;
        Ok(value)
    }

    pub(super) fn u32(&mut self, what: &str) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(
            self.take(4, what)?.try_into().expect("four bytes"),
        ))
    }

    pub(super) fn u64(&mut self, what: &str) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(
            self.take(8, what)?.try_into().expect("eight bytes"),
        ))
    }

    pub(super) fn u128(&mut self, what: &str) -> Result<u128, Error> {
        Ok(u128::from_le_bytes(
            self.take(16, what)?.try_into().expect("sixteen bytes"),
        ))
    }
}

/// The writing half: appends the fields in the order the layout lists them.
///
/// It is a type rather than a `Vec<u8>` because a layout whose next offset depends on what came
/// before has to ask what has been written so far. Recomputing that from the field sizes is the
/// same fact written twice, and the two spellings drift.
pub(super) struct Writer {
    out: Vec<u8>,
}

impl Writer {
    pub(super) fn new() -> Self {
        Writer { out: Vec::new() }
    }

    pub(super) fn with_capacity(capacity: usize) -> Self {
        Writer {
            out: Vec::with_capacity(capacity),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.out.len()
    }

    pub(super) fn bytes(&mut self, value: &[u8]) {
        self.out.extend_from_slice(value);
    }

    pub(super) fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub(super) fn u128(&mut self, value: u128) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    /// What has been written so far: the whole-file checksum covers exactly this.
    pub(super) fn as_bytes(&self) -> &[u8] {
        &self.out
    }

    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}
