//! Little-endian integers in a byte buffer.
//!
//! Every object format mirvm reads or writes — ELF, Mach-O, the `.mirvm` container — is
//! little-endian and laid out by fixed offsets, so the same few operations serve all of them: read
//! at an offset, write at an offset, and the reader-over-a-buffer form a parser walks with
//! (`&mut &[u8]`, advancing). The width is part of each name rather than a type parameter, so a call
//! site says how many bytes it moves.
//!
//! Nothing here decides what a failure means. A short buffer is `None`, and the caller maps it to the
//! sentence its own layer reports, which is why a truncated package says which field was cut and a
//! truncated object file names the loader that refused it.
//!
//! The writers are the exception, deliberately: an offset the caller computed from the layout it is
//! assembling is not input, so a buffer too short for it is a bug in that layout and panics here
//! rather than being silently dropped.
//!
//! A width appears below once a format needs it; add the next one with its caller.

/// The `N` bytes at `offset`, or `None` when the buffer is shorter.
fn field<const N: usize>(bytes: &[u8], offset: usize) -> Option<[u8; N]> {
    let end = offset.checked_add(N)?;
    bytes.get(offset..end)?.try_into().ok()
}

/// The first `N` bytes of `rest`, advancing it, or `None` when fewer are left.
fn take_field<const N: usize>(rest: &mut &[u8]) -> Option<[u8; N]> {
    let (field, tail) = rest.split_at_checked(N)?;
    *rest = tail;
    field.try_into().ok()
}

/// Put `bytes` at `offset`.
///
/// Both bounds are checked explicitly so that a wrapped range cannot become a write at the wrong
/// offset in a build without overflow checks.
fn write_field(out: &mut [u8], offset: usize, bytes: &[u8]) {
    let end = offset
        .checked_add(bytes.len())
        .expect("a write offset cannot overflow");
    out.get_mut(offset..end)
        .expect("a write offset must be inside the buffer")
        .copy_from_slice(bytes);
}

/// Read one little-endian `u16` at `offset`. `None` when the buffer is shorter than two bytes.
pub(crate) fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(field(bytes, offset)?))
}

/// Read one little-endian `u32` at `offset`. `None` when the buffer is shorter than four bytes.
pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(field(bytes, offset)?))
}

/// Read one little-endian `u64` at `offset`. `None` when the buffer is shorter than eight bytes.
pub(crate) fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(field(bytes, offset)?))
}

/// Write one little-endian `u16` at `offset`.
///
/// Panics when the buffer is shorter than the value: see the module note on why a caller-computed
/// offset is not input.
pub(crate) fn write_u16(out: &mut [u8], offset: usize, value: u16) {
    write_field(out, offset, &value.to_le_bytes());
}

/// Write one little-endian `u32` at `offset`. Panics as [`write_u16`] does.
pub(crate) fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    write_field(out, offset, &value.to_le_bytes());
}

/// Write one little-endian `u64` at `offset`. Panics as [`write_u16`] does.
pub(crate) fn write_u64(out: &mut [u8], offset: usize, value: u64) {
    write_field(out, offset, &value.to_le_bytes());
}

/// Take one little-endian `u32` off the front of `rest`, advancing it.
pub(crate) fn take_u32(rest: &mut &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(take_field(rest)?))
}

/// Take one little-endian `u64` off the front of `rest`, advancing it.
pub(crate) fn take_u64(rest: &mut &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(take_field(rest)?))
}

/// Take one little-endian `u128` off the front of `rest`, advancing it.
pub(crate) fn take_u128(rest: &mut &[u8]) -> Option<u128> {
    Some(u128::from_le_bytes(take_field(rest)?))
}

/// Take `len` bytes off the front of `rest`, advancing it. `None` when fewer are left.
pub(crate) fn take_bytes<'a>(rest: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    let (field, tail) = rest.split_at_checked(len)?;
    *rest = tail;
    Some(field)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every width round-trips, and each write lands at its own offset in little-endian order.
    #[test]
    fn little_endian_round_trip() {
        let mut buf = [0u8; 14];
        write_u16(&mut buf, 0, 0x0102);
        write_u32(&mut buf, 2, 0x0304_0506);
        write_u64(&mut buf, 6, 0x0708_090a_0b0c_0d0e);
        assert_eq!(buf, [2, 1, 6, 5, 4, 3, 14, 13, 12, 11, 10, 9, 8, 7]);

        assert_eq!(read_u16(&buf, 0), Some(0x0102));
        assert_eq!(read_u32(&buf, 2), Some(0x0304_0506));
        assert_eq!(read_u64(&buf, 6), Some(0x0708_090a_0b0c_0d0e));
        // A read may straddle fields: bytes 3..7 are 05 04 03 0e.
        assert_eq!(read_u32(&buf, 3), Some(0x0e03_0405));
    }

    /// The advancing form reads the same values and leaves the reader past them, including past the
    /// end, where the next read is simply `None`.
    #[test]
    fn advancing_reads_walk_the_buffer() {
        let mut buf = [0u8; 16];
        write_u32(&mut buf, 0, 0x0102_0304);
        write_u64(&mut buf, 4, 0x0506_0708_090a_0b0c);

        let mut rest: &[u8] = &buf;
        assert_eq!(take_u32(&mut rest), Some(0x0102_0304));
        assert_eq!(take_u64(&mut rest), Some(0x0506_0708_090a_0b0c));
        assert_eq!(take_bytes(&mut rest, 4), Some(&[0, 0, 0, 0][..]));
        assert!(rest.is_empty(), "the reader is past the end");
        assert_eq!(take_u32(&mut rest), None);
    }

    /// A sixteen-byte read spans the two eight-byte writes that built it.
    #[test]
    fn wide_read_spans_narrow_writes() {
        let mut buf = [0u8; 16];
        write_u64(&mut buf, 0, 0x0708_090a_0b0c_0d0e);
        write_u64(&mut buf, 8, 0x0102_0304_0506_0708);

        let mut rest: &[u8] = &buf;
        assert_eq!(
            take_u128(&mut rest),
            Some(0x0102_0304_0506_0708_0708_090a_0b0c_0d0e)
        );
    }

    /// A short buffer is `None` from both reader forms, and a refused take leaves the reader where it
    /// was rather than advancing past what it could not read.
    #[test]
    fn short_input_is_none() {
        let buf = [0u8; 3];
        assert_eq!(read_u32(&buf, 0), None);
        assert_eq!(read_u16(&buf, 2), None);
        assert_eq!(read_u16(&buf, usize::MAX), None);
        assert_eq!(read_u64(&buf, 0), None);

        let mut rest: &[u8] = &buf;
        assert_eq!(take_u32(&mut rest), None);
        assert_eq!(take_bytes(&mut rest, 4), None);
        assert_eq!(rest, &buf[..]);
        assert_eq!(take_bytes(&mut rest, 3), Some(&buf[..]));
        assert!(rest.is_empty());
    }
}
