//! The FUNCS section: one postcard-encoded body per function behind an index of offsets, lengths and
//! digests. The index is addressed by function id, so its order is the module's function order.

use crate::vm::ir::{FuncBlob, FuncTable};

use super::Error;
use super::bytes::{Cursor, Writer};
use super::format::{check_hash, hash128};
use super::meta::postcard_bytes;

/// One index entry: offset, length, digest.
pub(super) const FUNC_ENTRY_LEN: usize = 32;

pub(super) fn build_function_section(funcs: &FuncTable) -> Result<Vec<u8>, Error> {
    let count =
        u32::try_from(funcs.len()).map_err(|_| Error::build("too many functions in package"))?;
    let table_len = funcs
        .len()
        .checked_mul(FUNC_ENTRY_LEN)
        .and_then(|len| len.checked_add(4))
        .ok_or_else(|| Error::build("package function table is too large"))?;
    let mut encoded = Vec::with_capacity(funcs.len());
    let mut offset = table_len;
    for body in funcs {
        let bytes = postcard_bytes(body)?;
        let end = offset
            .checked_add(bytes.len())
            .ok_or_else(|| Error::build("package function data is too large"))?;
        encoded.push((offset, bytes));
        offset = end;
    }
    let mut out = Writer::with_capacity(offset);
    out.u32(count);
    for (offset, bytes) in &encoded {
        out.u64(
            u64::try_from(*offset)
                .map_err(|_| Error::build("package function offset is too large"))?,
        );
        out.u64(
            u64::try_from(bytes.len())
                .map_err(|_| Error::build("package function is too large"))?,
        );
        out.u128(hash128(bytes));
    }
    for (_, bytes) in encoded {
        out.bytes(&bytes);
    }
    Ok(out.into_bytes())
}

pub(super) fn parse_function_section(
    section: &[u8],
    mapped_offset: usize,
) -> Result<Vec<FuncBlob>, Error> {
    let mut cursor = Cursor::new(section, 0);
    let count = usize::try_from(cursor.u32("function count")?)
        .map_err(|_| Error::corrupt("package function count does not fit this host"))?;
    let table_end = count
        .checked_mul(FUNC_ENTRY_LEN)
        .and_then(|len| len.checked_add(4))
        .ok_or_else(|| Error::corrupt("package function table length overflow"))?;
    if table_end > section.len() {
        return Err(Error::corrupt(
            "package function table is truncated or too large",
        ));
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count)
        .map_err(|_| Error::corrupt("package function index is too large for available memory"))?;
    for index in 0..count {
        let start = usize::try_from(cursor.u64("function offset")?).map_err(|_| {
            Error::corrupt(format!("function {index} offset does not fit this host"))
        })?;
        let len = usize::try_from(cursor.u64("function length")?).map_err(|_| {
            Error::corrupt(format!("function {index} length does not fit this host"))
        })?;
        let expected_hash = cursor.u128("function hash")?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| Error::corrupt(format!("function {index} range overflow")))?;
        if start < table_end || end > section.len() {
            return Err(Error::corrupt(format!(
                "function {index} crossed FUNCS section boundary"
            )));
        }
        check_hash(
            &section[start..end],
            expected_hash,
            format!("function {index} has wrong hash"),
        )?;
        entries.push((index, start, end, expected_hash));
    }
    // The overlap scan needs offset order; the blobs are indexed by function id, so the table is
    // sorted for the scan and restored after it.
    entries.sort_unstable_by_key(|entry| entry.1);
    if let Some(pair) = entries.windows(2).find(|pair| pair[1].1 < pair[0].2) {
        return Err(Error::corrupt(format!(
            "functions {} and {} overlap in FUNCS section",
            pair[0].0, pair[1].0
        )));
    }
    entries.sort_unstable_by_key(|entry| entry.0);
    entries
        .into_iter()
        .map(|(_, start, end, expected_hash)| {
            Ok(FuncBlob {
                start: mapped_offset
                    .checked_add(start)
                    .ok_or_else(|| Error::build("mapped function offset overflow"))?,
                end: mapped_offset
                    .checked_add(end)
                    .ok_or_else(|| Error::build("mapped function end overflow"))?,
                expected_hash,
            })
        })
        .collect()
}
