//! The `.mirvm` container: its magic, version, section table and checksums, and the cursors both
//! directions share. The layout is documented on the module above; this file owns its bytes.

use std::collections::HashSet;

use super::Error;

/// Container magic: eight bytes, and the sniffer's whole criterion.
pub(super) const MAGIC: &[u8; 8] = b"MIRVMAR\0";
/// Format generation. A different value makes a package incompatible rather than corrupt.
pub(super) const FMT_VER: u32 = 4;
/// One section table entry: tag, offset, length, checksum.
pub(super) const SECTION_ENTRY_LEN: usize = 36;
/// The whole-file checksum trailer.
pub(super) const WHOLE_HASH_LEN: usize = 16;

pub(super) const TAG_META: u32 = 1;
pub(super) const TAG_STAMPS: u32 = 2;
/// The BASE section is reserved for the delta form; modules are always full, so it is never written.
pub(super) const TAG_BASE: u32 = 3;
pub(super) const TAG_MODULE: u32 = 4;
pub(super) const TAG_NATIVELIBS: u32 = 5;
pub(super) const TAG_RELOC: u32 = 6;
pub(super) const TAG_MC: u32 = 7;
pub(super) const TAG_FUNCS: u32 = 8;

/// fnv1a-128: two passes with different seeds, the same checksum family as L2.
pub(super) fn hash128(data: &[u8]) -> u128 {
    let a = crate::utils::content::fnv1a(data);
    let mut b = 0xcbf2_9ce4_8422_2325u64;
    for byte in b"\x01mirvmar".iter().chain(data) {
        b ^= u64::from(*byte);
        b = b.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ((a as u128) << 64) | b as u128
}

/// Refuse a blob whose content no longer hashes to the digest recorded for it. The label is the
/// whole message, because the callers name different things and all of them mean it is unusable.
pub(super) fn check_hash(
    bytes: &[u8],
    recorded: u128,
    label: impl Into<String>,
) -> Result<(), Error> {
    if hash128(bytes) == recorded {
        Ok(())
    } else {
        Err(Error::corrupt(label))
    }
}

/// A reader over a byte slice that never runs off the end; a failure names what was being read.
pub(super) struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(super) fn new(data: &'a [u8], pos: usize) -> Self {
        Cursor { data, pos }
    }

    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8], Error> {
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

pub(super) struct ParsedPackage<'a> {
    pub(super) sections: Vec<(u32, &'a [u8])>,
}

impl<'a> ParsedPackage<'a> {
    pub(super) fn section(&self, tag: u32) -> Result<&'a [u8], Error> {
        self.sections
            .iter()
            .find_map(|(found, data)| (*found == tag).then_some(*data))
            .ok_or_else(|| Error::corrupt(format!("package must have section with tag={tag}")))
    }

    pub(super) fn has_section(&self, tag: u32) -> bool {
        self.sections.iter().any(|(found, _)| *found == tag)
    }
}

pub(super) fn build_container(sections: &[(u32, Vec<u8>)]) -> Result<Vec<u8>, Error> {
    let bid = crate::options::build::BUILD_ID.as_bytes();
    let bid_len =
        u32::try_from(bid.len()).map_err(|_| Error::build("package build_id too long"))?;
    let section_count =
        u32::try_from(sections.len()).map_err(|_| Error::build("too many sections in package"))?;
    let table_len = sections
        .len()
        .checked_mul(SECTION_ENTRY_LEN)
        .ok_or_else(|| Error::build("package section table is too large"))?;
    let data_start = MAGIC
        .len()
        .checked_add(4 + 4)
        .and_then(|n| n.checked_add(bid.len()))
        .and_then(|n| n.checked_add(4))
        .and_then(|n| n.checked_add(table_len))
        .ok_or_else(|| Error::build("package size overflow"))?;

    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&FMT_VER.to_le_bytes());
    buf.extend_from_slice(&bid_len.to_le_bytes());
    buf.extend_from_slice(bid);
    buf.extend_from_slice(&section_count.to_le_bytes());

    let mut off = u64::try_from(data_start).map_err(|_| Error::build("package offset overflow"))?;
    for (tag, data) in sections {
        let len =
            u64::try_from(data.len()).map_err(|_| Error::build("package section is too large"))?;
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&off.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&hash128(data).to_le_bytes());
        off = off
            .checked_add(len)
            .ok_or_else(|| Error::build("package size overflow"))?;
    }
    for (_, data) in sections {
        buf.extend_from_slice(data);
    }
    let whole = hash128(&buf);
    buf.extend_from_slice(&whole.to_le_bytes());
    Ok(buf)
}

pub(super) fn parse_container(raw: &[u8]) -> Result<ParsedPackage<'_>, Error> {
    const MIN_LEN: usize = 8 + 4 + 4 + 4 + WHOLE_HASH_LEN;
    if raw.len() < MIN_LEN || raw.get(..MAGIC.len()) != Some(MAGIC) {
        return Err(Error::not_a_package(
            "not .mirvm package (mismatched or truncated magic header)",
        ));
    }
    let body_len = raw
        .len()
        .checked_sub(WHOLE_HASH_LEN)
        .ok_or_else(|| Error::corrupt("package is shorter than its hash trailer"))?;
    let (body, whole) = raw.split_at(body_len);
    let recorded_hash = u128::from_le_bytes(whole.try_into().expect("sixteen-byte trailer"));
    check_hash(
        body,
        recorded_hash,
        "package content hash mismatched (broken or truncated)",
    )?;

    let mut cur = Cursor::new(body, MAGIC.len());
    let package_ver = cur.u32("format version")?;
    if package_ver != FMT_VER {
        return Err(Error::incompatible(format!(
            "wrong package format version (package={package_ver}, mirvm={FMT_VER})"
        )));
    }
    let bid_len = usize::try_from(cur.u32("build_id length")?)
        .map_err(|_| Error::corrupt("package build_id length does not fit this host"))?;
    let bid = std::str::from_utf8(cur.take(bid_len, "build_id")?)
        .map_err(|e| Error::corrupt(format!("invalid package build_id: {e}")))?;
    if bid != crate::options::build::BUILD_ID {
        return Err(Error::incompatible(
            "package build_id mismatch with current mirvm",
        ));
    }

    let count = usize::try_from(cur.u32("section count")?)
        .map_err(|_| Error::corrupt("package section count does not fit this host"))?;
    let table_len = count
        .checked_mul(SECTION_ENTRY_LEN)
        .ok_or_else(|| Error::corrupt("package section table length overflow"))?;
    let data_start = cur
        .pos
        .checked_add(table_len)
        .filter(|end| *end <= body.len())
        .ok_or_else(|| Error::corrupt("package section table is truncated or too large"))?;

    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count)
        .map_err(|_| Error::corrupt("package section table is too large for available memory"))?;
    let mut tags = HashSet::new();
    tags.try_reserve(count).map_err(|_| {
        Error::corrupt("package section tag table is too large for available memory")
    })?;
    for index in 0..count {
        let tag = cur.u32("section tag")?;
        if !tags.insert(tag) {
            return Err(Error::corrupt(format!(
                "package has duplicate section tag={tag}"
            )));
        }
        let off = usize::try_from(cur.u64("section offset")?).map_err(|_| {
            Error::corrupt(format!(
                "package section {index} offset does not fit this host"
            ))
        })?;
        let len = usize::try_from(cur.u64("section length")?).map_err(|_| {
            Error::corrupt(format!(
                "package section {index} length does not fit this host"
            ))
        })?;
        let expected_hash = cur.u128("section hash")?;
        let end = off.checked_add(len).ok_or_else(|| {
            Error::corrupt(format!("package section with tag={tag} range overflow"))
        })?;
        if off < data_start || end > body.len() {
            return Err(Error::corrupt(format!(
                "package section with tag={tag} crossed its boundary"
            )));
        }
        entries.push((tag, off, end, expected_hash));
    }

    drop(tags);
    entries.sort_unstable_by_key(|(_, start, _, _)| *start);
    for pair in entries.windows(2) {
        if pair[1].1 < pair[0].2 {
            return Err(Error::corrupt(format!(
                "package sections with tag={} and tag={} overlap",
                pair[0].0, pair[1].0
            )));
        }
    }

    let mut sections = Vec::new();
    sections
        .try_reserve_exact(entries.len())
        .map_err(|_| Error::corrupt("package section index is too large for available memory"))?;
    for (tag, start, end, expected_hash) in entries {
        let data = &body[start..end];
        check_hash(
            data,
            expected_hash,
            format!("package section with tag={tag} has wrong hash value"),
        )?;
        sections.push((tag, data));
    }
    Ok(ParsedPackage { sections })
}
