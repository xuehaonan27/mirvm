//! What a capture file is made of: the file header, and the chunk framing around each sealed group
//! of pages.
//!
//! A reader that only wants to walk the file — find the chunks, check their digests, skip to the
//! ledger — needs nothing from the record modules beside this one.

use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileHeader {
    pub(crate) pointer_width: u8,
    pub(crate) clock_kind: u8,
    pub(crate) pid: u32,
    pub(crate) session_id: [u8; 16],
    pub(crate) build_id: u64,
    pub(crate) process_generation: u64,
    pub(crate) segment_number: u64,
    pub(crate) monotonic_anchor: u64,
    pub(crate) wall_unix_ns: u64,
    pub(crate) clock_frequency_num: u64,
    pub(crate) clock_frequency_den: u64,
}

impl FileHeader {
    pub(crate) fn to_le_bytes(&self) -> [u8; FILE_HEADER_BYTES] {
        let mut out = [0_u8; FILE_HEADER_BYTES];
        put(&mut out, 0, FILE_MAGIC);
        put_u16(&mut out, 8, FORMAT_MAJOR);
        put_u16(&mut out, 10, FORMAT_MINOR);
        put_u32(&mut out, 12, FILE_HEADER_BYTES as u32);
        put_u16(&mut out, 16, SCHEMA_MAJOR);
        put_u16(&mut out, 18, SCHEMA_MINOR);
        out[20] = self.pointer_width;
        out[21] = self.clock_kind;
        put_u32(&mut out, 24, self.pid);
        put(&mut out, 32, &self.session_id);
        put_u64(&mut out, 48, self.build_id);
        put_u64(&mut out, 56, self.process_generation);
        put_u64(&mut out, 64, self.segment_number);
        put_u64(&mut out, 72, self.monotonic_anchor);
        put_u64(&mut out, 80, self.wall_unix_ns);
        put_u64(&mut out, 88, self.clock_frequency_num);
        put_u64(&mut out, 96, self.clock_frequency_den);
        let digest = blake3::hash(&out[..128]);
        put(&mut out, 128, digest.as_bytes());
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, FILE_HEADER_BYTES, "file header")?;
        require_magic(bytes, FILE_MAGIC, "file")?;
        require_version(bytes, 8, "file")?;
        require_eq_u32(bytes, 12, FILE_HEADER_BYTES as u32, "file header length")?;
        require_eq_u16(bytes, 16, SCHEMA_MAJOR, "event schema major")?;
        require_eq_u16(bytes, 18, SCHEMA_MINOR, "event schema minor")?;
        require_zero(bytes, 22..24, "file flags")?;
        require_zero(bytes, 28..32, "file reserved field")?;
        require_zero(bytes, 104..128, "file reserved bytes")?;

        let expected = blake3::hash(&bytes[..128]);
        if bytes[128..160] != expected.as_bytes()[..] {
            return Err(WireError::new("file header checksum mismatch"));
        }

        let pointer_width = bytes[20];
        if pointer_width != 8 {
            return Err(WireError::new(format!(
                "unsupported v0 pointer width {pointer_width}"
            )));
        }
        let clock_kind = bytes[21];
        if clock_kind != CLOCK_NONE {
            return Err(WireError::new(format!(
                "unsupported v0 clock kind {clock_kind}"
            )));
        }
        let monotonic_anchor = read_u64(bytes, 72, "monotonic anchor")?;
        let wall_unix_ns = read_u64(bytes, 80, "wall clock anchor")?;
        let clock_frequency_num = read_u64(bytes, 88, "clock frequency numerator")?;
        let clock_frequency_den = read_u64(bytes, 96, "clock frequency denominator")?;
        if [
            monotonic_anchor,
            wall_unix_ns,
            clock_frequency_num,
            clock_frequency_den,
        ]
        .iter()
        .any(|value| *value != 0)
        {
            return Err(WireError::new("no-time v0 file has non-zero clock anchors"));
        }
        let segment_number = read_u64(bytes, 64, "segment number")?;
        if segment_number != 0 {
            return Err(WireError::new(format!(
                "v0 does not support segment number {segment_number}"
            )));
        }
        Ok(Self {
            pointer_width,
            clock_kind,
            pid: read_u32(bytes, 24, "pid")?,
            session_id: read_array(bytes, 32, "session id")?,
            build_id: read_u64(bytes, 48, "build id")?,
            process_generation: read_u64(bytes, 56, "process generation")?,
            segment_number,
            monotonic_anchor,
            wall_unix_ns,
            clock_frequency_num,
            clock_frequency_den,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChunkHeader {
    pub(crate) chunk_ordinal: u64,
    pub(crate) payload_bytes: u64,
    pub(crate) block_count: u32,
    pub(crate) page_count: u32,
}

impl ChunkHeader {
    pub(crate) fn new(
        chunk_ordinal: u64,
        payload_bytes: u64,
        block_count: u32,
        page_count: u32,
    ) -> Result<Self, WireError> {
        let header = Self {
            chunk_ordinal,
            payload_bytes,
            block_count,
            page_count,
        };
        header.validate()?;
        Ok(header)
    }

    fn validate(&self) -> Result<(), WireError> {
        if self.payload_bytes > MAX_CHUNK_PAYLOAD_BYTES {
            return Err(WireError::new(format!(
                "chunk payload {} exceeds v0 maximum {MAX_CHUNK_PAYLOAD_BYTES}",
                self.payload_bytes
            )));
        }
        if self.page_count > self.block_count {
            return Err(WireError::new("chunk page count exceeds block count"));
        }
        if (self.payload_bytes == 0) != (self.block_count == 0) {
            return Err(WireError::new(
                "chunk payload and block count disagree about emptiness",
            ));
        }
        Ok(())
    }

    pub(crate) fn to_le_bytes(&self) -> [u8; CHUNK_HEADER_BYTES] {
        let mut out = [0_u8; CHUNK_HEADER_BYTES];
        put(&mut out, 0, CHUNK_MAGIC);
        put_u16(&mut out, 8, FORMAT_MAJOR);
        put_u16(&mut out, 10, FORMAT_MINOR);
        put_u32(&mut out, 12, CHUNK_HEADER_BYTES as u32);
        put_u64(&mut out, 16, self.chunk_ordinal);
        put_u64(&mut out, 24, self.payload_bytes);
        put_u32(&mut out, 32, self.block_count);
        put_u32(&mut out, 36, self.page_count);
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, CHUNK_HEADER_BYTES, "chunk header")?;
        require_magic(bytes, CHUNK_MAGIC, "chunk")?;
        require_version(bytes, 8, "chunk")?;
        require_eq_u32(bytes, 12, CHUNK_HEADER_BYTES as u32, "chunk header length")?;
        require_zero(bytes, 40..64, "chunk flags/reserved bytes")?;
        Self::new(
            read_u64(bytes, 16, "chunk ordinal")?,
            read_u64(bytes, 24, "chunk payload length")?,
            read_u32(bytes, 32, "chunk block count")?,
            read_u32(bytes, 36, "chunk page count")?,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChunkFooter {
    pub(crate) chunk_ordinal: u64,
    pub(crate) payload_bytes: u64,
    pub(crate) digest: [u8; 32],
}

impl ChunkFooter {
    pub(crate) fn for_slices<'a>(
        header: &ChunkHeader,
        encoded_header: &[u8; CHUNK_HEADER_BYTES],
        payload: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Self, WireError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(encoded_header);
        let mut payload_bytes = 0_u64;
        for slice in payload {
            payload_bytes = payload_bytes
                .checked_add(
                    u64::try_from(slice.len())
                        .map_err(|_| WireError::new("chunk payload length does not fit u64"))?,
                )
                .ok_or_else(|| WireError::new("chunk payload length overflow"))?;
            hasher.update(slice);
        }
        if payload_bytes != header.payload_bytes {
            return Err(WireError::new(format!(
                "chunk payload length mismatch (header={}, slices={payload_bytes})",
                header.payload_bytes
            )));
        }
        Ok(Self {
            chunk_ordinal: header.chunk_ordinal,
            payload_bytes,
            digest: *hasher.finalize().as_bytes(),
        })
    }

    pub(crate) fn to_le_bytes(&self) -> [u8; CHUNK_FOOTER_BYTES] {
        let mut out = [0_u8; CHUNK_FOOTER_BYTES];
        put(&mut out, 0, COMMIT_MAGIC);
        put_u16(&mut out, 8, FORMAT_MAJOR);
        put_u16(&mut out, 10, FORMAT_MINOR);
        put_u32(&mut out, 12, CHUNK_FOOTER_BYTES as u32);
        put_u64(&mut out, 16, self.chunk_ordinal);
        put_u64(&mut out, 24, self.payload_bytes);
        put(&mut out, 32, &self.digest);
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, CHUNK_FOOTER_BYTES, "chunk footer")?;
        require_magic(bytes, COMMIT_MAGIC, "chunk commit")?;
        require_version(bytes, 8, "chunk commit")?;
        require_eq_u32(bytes, 12, CHUNK_FOOTER_BYTES as u32, "chunk footer length")?;
        Ok(Self {
            chunk_ordinal: read_u64(bytes, 16, "committed chunk ordinal")?,
            payload_bytes: read_u64(bytes, 24, "committed payload length")?,
            digest: read_array(bytes, 32, "chunk digest")?,
        })
    }

    pub(crate) fn verify(
        &self,
        header: &ChunkHeader,
        encoded_header: &[u8; CHUNK_HEADER_BYTES],
        payload: &[u8],
    ) -> Result<(), WireError> {
        if self.chunk_ordinal != header.chunk_ordinal || self.payload_bytes != header.payload_bytes
        {
            return Err(WireError::new(
                "chunk footer does not repeat header ordinal and length",
            ));
        }
        let expected = Self::for_slices(header, encoded_header, [payload])?;
        if self.digest != expected.digest {
            return Err(WireError::new("chunk checksum mismatch"));
        }
        Ok(())
    }
}
