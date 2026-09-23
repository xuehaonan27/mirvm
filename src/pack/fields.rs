//! The container's fields, read with the name of the field in the refusal.
//!
//! std already has the pieces: `from_le_bytes` converts, `split_first_chunk` bounds-checks, and a
//! `&mut &[u8]` is the ordinary reader-over-a-buffer idiom. What std has no shape for is this
//! format's refusal — an artifact that ends mid-field has to say *which* field was cut, because that
//! sentence is all the user gets when their package is truncated — so these are that sentence, once,
//! for the eleven fields the container and the FUNCS section read.

use super::Error;

/// Take `len` bytes, advancing the reader.
pub(super) fn take<'a>(rest: &mut &'a [u8], len: usize, what: &str) -> Result<&'a [u8], Error> {
    if len > rest.len() {
        return Err(Error::corrupt(format!(
            "package truncated while reading {what}"
        )));
    }
    let (head, tail) = rest.split_at(len);
    *rest = tail;
    Ok(head)
}

pub(super) fn u32(rest: &mut &[u8], what: &str) -> Result<u32, Error> {
    let (field, tail) = rest
        .split_first_chunk::<4>()
        .ok_or_else(|| Error::corrupt(format!("package truncated while reading {what}")))?;
    *rest = tail;
    Ok(u32::from_le_bytes(*field))
}

pub(super) fn u64(rest: &mut &[u8], what: &str) -> Result<u64, Error> {
    let (field, tail) = rest
        .split_first_chunk::<8>()
        .ok_or_else(|| Error::corrupt(format!("package truncated while reading {what}")))?;
    *rest = tail;
    Ok(u64::from_le_bytes(*field))
}

pub(super) fn u128(rest: &mut &[u8], what: &str) -> Result<u128, Error> {
    let (field, tail) = rest
        .split_first_chunk::<16>()
        .ok_or_else(|| Error::corrupt(format!("package truncated while reading {what}")))?;
    *rest = tail;
    Ok(u128::from_le_bytes(*field))
}
