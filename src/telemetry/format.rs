//! v0 binary format shared by the capture writer and offline tools.
//!
//! Wire bytes are always encoded explicitly as little endian. Rust struct layout is
//! deliberately not part of the file contract.

use std::fmt;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WireError(String);

impl WireError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WireError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Control {
    pub(crate) kind: u16,
    pub(crate) version: u8,
    pub(crate) flags: u8,
    pub(crate) length_qwords: u16,
}

impl Control {
    pub(crate) const fn new(kind: u16, flags: u8, length_bytes: usize) -> Self {
        assert!(length_bytes > 0 && length_bytes.is_multiple_of(8));
        assert!(length_bytes / 8 <= u16::MAX as usize);
        Self {
            kind,
            version: 0,
            flags,
            length_qwords: (length_bytes / 8) as u16,
        }
    }

    pub(crate) const fn byte_len(self) -> usize {
        self.length_qwords as usize * 8
    }

    pub(crate) const fn word(self) -> u64 {
        (self.kind as u64)
            | ((self.version as u64) << 16)
            | ((self.flags as u64) << 24)
            | ((self.length_qwords as u64) << 32)
    }

    pub(crate) const fn to_le_bytes(self) -> [u8; 8] {
        self.word().to_le_bytes()
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let word = read_u64(bytes, 0, "control")?;
        let reserved = (word >> 48) as u16;
        if reserved != 0 {
            return Err(WireError::new(format!(
                "control reserved bits are non-zero: 0x{reserved:04x}"
            )));
        }
        let flags = (word >> 24) as u8;
        if flags & !KNOWN_CONTROL_FLAGS != 0 {
            return Err(WireError::new(format!(
                "control has unknown v0 flags: 0x{flags:02x}"
            )));
        }
        let control = Self {
            kind: word as u16,
            version: (word >> 16) as u8,
            flags,
            length_qwords: (word >> 32) as u16,
        };
        if control.length_qwords == 0 {
            return Err(WireError::new("record length is zero"));
        }
        Ok(control)
    }
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PageHeader {
    pub(crate) producer_id: u64,
    pub(crate) first_sequence: u64,
    /// Exclusive next sequence at seal. An active, unpublished page may keep this zero.
    pub(crate) next_sequence: u64,
    pub(crate) initial_engine_id: u64,
    pub(crate) page_ordinal: u64,
    pub(crate) thread_generation: u32,
    pub(crate) os_tid: u32,
    pub(crate) page_bytes: u32,
    /// Payload bytes at seal. An active, unpublished page may keep this zero.
    pub(crate) used_bytes: u32,
}

impl PageHeader {
    pub(crate) fn to_le_bytes(&self) -> Result<[u8; PAGE_HEADER_BYTES], WireError> {
        self.validate()?;
        let encoded_bytes = PAGE_HEADER_BYTES
            .checked_add(self.used_bytes as usize)
            .ok_or_else(|| WireError::new("page encoded length overflow"))?;
        let mut out = [0_u8; PAGE_HEADER_BYTES];
        put(
            &mut out,
            0,
            &Control::new(KIND_PAGE, 0, encoded_bytes).to_le_bytes(),
        );
        put_u64(&mut out, 8, self.producer_id);
        put_u64(&mut out, 16, self.first_sequence);
        put_u64(&mut out, 24, self.next_sequence);
        put_u64(&mut out, 32, self.initial_engine_id);
        put_u64(&mut out, 40, self.page_ordinal);
        put_u32(&mut out, 48, self.thread_generation);
        put_u32(&mut out, 52, self.os_tid);
        put_u32(&mut out, 56, self.page_bytes);
        put_u32(&mut out, 60, self.used_bytes);
        Ok(out)
    }

    fn validate(&self) -> Result<(), WireError> {
        if !matches!(
            self.page_bytes,
            PAGE_BYTES_4K | PAGE_BYTES_16K | PAGE_BYTES_64K
        ) {
            return Err(WireError::new(format!(
                "unsupported logical page size {}",
                self.page_bytes
            )));
        }
        if self.used_bytes == 0 || !self.used_bytes.is_multiple_of(8) {
            return Err(WireError::new(
                "sealed page payload must be non-zero and eight-byte aligned",
            ));
        }
        let encoded_bytes = PAGE_HEADER_BYTES as u64 + u64::from(self.used_bytes);
        if encoded_bytes > u64::from(self.page_bytes) {
            return Err(WireError::new(format!(
                "page header plus used bytes ({encoded_bytes}) exceeds logical page size {}",
                self.page_bytes
            )));
        }
        if self.next_sequence < self.first_sequence {
            return Err(WireError::new(
                "sealed page next sequence precedes first sequence",
            ));
        }
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, PAGE_HEADER_BYTES, "page header")?;
        let control = Control::decode(bytes)?;
        if control.kind != KIND_PAGE || control.version != 0 || control.flags != 0 {
            return Err(WireError::new("payload block is not a v0 page"));
        }
        let header = Self {
            producer_id: read_u64(bytes, 8, "producer id")?,
            first_sequence: read_u64(bytes, 16, "first sequence")?,
            next_sequence: read_u64(bytes, 24, "next sequence")?,
            initial_engine_id: read_u64(bytes, 32, "initial engine id")?,
            page_ordinal: read_u64(bytes, 40, "page ordinal")?,
            thread_generation: read_u32(bytes, 48, "thread generation")?,
            os_tid: read_u32(bytes, 52, "OS tid")?,
            page_bytes: read_u32(bytes, 56, "logical page bytes")?,
            used_bytes: read_u32(bytes, 60, "used page bytes")?,
        };
        header.validate()?;
        if control.byte_len() != PAGE_HEADER_BYTES + header.used_bytes as usize {
            return Err(WireError::new(
                "page control length disagrees with used bytes",
            ));
        }
        Ok(header)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SyscallSemantics {
    Raw,
    Libc,
}

impl SyscallSemantics {
    const fn flag(self) -> u8 {
        match self {
            Self::Raw => FLAG_RAW,
            Self::Libc => FLAG_LIBC,
        }
    }

    fn from_control(control: Control, expected_kind: u16) -> Result<Self, WireError> {
        if control.kind != expected_kind
            || control.version != 0
            || control.flags & FLAG_CONTEXT_CONTROL != 0
        {
            return Err(WireError::new(
                "invalid syscall record kind or context flag",
            ));
        }
        match control.flags & (FLAG_RAW | FLAG_LIBC) {
            FLAG_RAW => Ok(Self::Raw),
            FLAG_LIBC => Ok(Self::Libc),
            _ => Err(WireError::new(
                "syscall record must set exactly one of RAW and LIBC",
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyscallEnter {
    pub(crate) semantics: SyscallSemantics,
    pub(crate) nr: i64,
    pub(crate) args: [u64; 6],
}

impl SyscallEnter {
    pub(crate) fn to_le_bytes(&self) -> [u8; SYSCALL_ENTER_BYTES] {
        let mut out = [0_u8; SYSCALL_ENTER_BYTES];
        put(
            &mut out,
            0,
            &Control::new(
                KIND_SYSCALL_ENTER,
                self.semantics.flag(),
                SYSCALL_ENTER_BYTES,
            )
            .to_le_bytes(),
        );
        put_i64(&mut out, 8, self.nr);
        for (index, arg) in self.args.iter().enumerate() {
            put_u64(&mut out, 16 + index * 8, *arg);
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, SYSCALL_ENTER_BYTES, "SyscallEnter")?;
        let control = Control::decode(bytes)?;
        if control.byte_len() != SYSCALL_ENTER_BYTES {
            return Err(WireError::new("SyscallEnter has wrong record length"));
        }
        let semantics = SyscallSemantics::from_control(control, KIND_SYSCALL_ENTER)?;
        let mut args = [0_u64; 6];
        for (index, arg) in args.iter_mut().enumerate() {
            *arg = read_u64(bytes, 16 + index * 8, "syscall argument")?;
        }
        Ok(Self {
            semantics,
            nr: read_i64(bytes, 8, "syscall number")?,
            args,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyscallExit {
    pub(crate) semantics: SyscallSemantics,
    pub(crate) result: i64,
    pub(crate) errno: Option<u32>,
}

impl SyscallExit {
    pub(crate) fn to_le_bytes(&self) -> Result<[u8; SYSCALL_EXIT_BYTES], WireError> {
        self.validate()?;
        let mut out = [0_u8; SYSCALL_EXIT_BYTES];
        put(
            &mut out,
            0,
            &Control::new(KIND_SYSCALL_EXIT, self.semantics.flag(), SYSCALL_EXIT_BYTES)
                .to_le_bytes(),
        );
        put_i64(&mut out, 8, self.result);
        let status = self
            .errno
            .map_or(0, |errno| u64::from(errno) | STATUS_ERRNO_VALID);
        put_u64(&mut out, 16, status);
        Ok(out)
    }

    fn validate(&self) -> Result<(), WireError> {
        match self.semantics {
            SyscallSemantics::Raw if self.errno.is_none() => Ok(()),
            SyscallSemantics::Raw => Err(WireError::new("raw SyscallExit cannot carry libc errno")),
            SyscallSemantics::Libc if self.result == -1 && self.errno.is_some() => Ok(()),
            SyscallSemantics::Libc if self.result == -1 => Err(WireError::new(
                "failed libc SyscallExit must carry a valid errno",
            )),
            SyscallSemantics::Libc if self.errno.is_none() => Ok(()),
            SyscallSemantics::Libc => Err(WireError::new(
                "successful libc SyscallExit must have zero status",
            )),
        }
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, SYSCALL_EXIT_BYTES, "SyscallExit")?;
        let control = Control::decode(bytes)?;
        if control.byte_len() != SYSCALL_EXIT_BYTES {
            return Err(WireError::new("SyscallExit has wrong record length"));
        }
        let semantics = SyscallSemantics::from_control(control, KIND_SYSCALL_EXIT)?;
        let result = read_i64(bytes, 8, "syscall result")?;
        let status = read_u64(bytes, 16, "syscall status")?;
        if status & !STATUS_KNOWN_MASK != 0 {
            return Err(WireError::new(format!(
                "SyscallExit status has reserved bits: 0x{status:016x}"
            )));
        }
        let errno = (status & STATUS_ERRNO_VALID != 0).then_some(status as u32);
        if errno.is_none() && status & STATUS_ERRNO_MASK != 0 {
            return Err(WireError::new(
                "SyscallExit has errno bits without errno_valid",
            ));
        }
        let exit = Self {
            semantics,
            result,
            errno,
        };
        exit.validate()?;
        Ok(exit)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EngineContext {
    pub(crate) engine_id: u64,
}

impl EngineContext {
    pub(crate) fn to_le_bytes(&self) -> [u8; ENGINE_CONTEXT_BYTES] {
        let mut out = [0_u8; ENGINE_CONTEXT_BYTES];
        put(
            &mut out,
            0,
            &Control::new(
                KIND_ENGINE_CONTEXT,
                FLAG_CONTEXT_CONTROL,
                ENGINE_CONTEXT_BYTES,
            )
            .to_le_bytes(),
        );
        put_u64(&mut out, 8, self.engine_id);
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, ENGINE_CONTEXT_BYTES, "EngineContext")?;
        let control = Control::decode(bytes)?;
        if control.kind != KIND_ENGINE_CONTEXT
            || control.version != 0
            || control.flags != FLAG_CONTEXT_CONTROL
            || control.byte_len() != ENGINE_CONTEXT_BYTES
        {
            return Err(WireError::new("invalid EngineContext control"));
        }
        Ok(Self {
            engine_id: read_u64(bytes, 8, "Engine id")?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProducerEnd {
    pub(crate) producer_id: u64,
    pub(crate) next_sequence: u64,
    pub(crate) attempted: u64,
    pub(crate) encoded: u64,
    pub(crate) committed: u64,
    pub(crate) drop_capacity: u64,
    pub(crate) drop_context: u64,
    pub(crate) drop_recursive: u64,
    pub(crate) sink_loss: u64,
    pub(crate) page_count: u64,
    pub(crate) last_page_ordinal: u64,
    pub(crate) thread_generation: u32,
    pub(crate) os_tid: u32,
    pub(crate) flags: u64,
}

impl ProducerEnd {
    pub(crate) fn to_le_bytes(&self) -> Result<[u8; PRODUCER_END_BYTES], WireError> {
        self.validate()?;
        let mut out = [0_u8; PRODUCER_END_BYTES];
        put(
            &mut out,
            0,
            &Control::new(KIND_PRODUCER_END, 0, PRODUCER_END_BYTES).to_le_bytes(),
        );
        put_u64(&mut out, 8, self.producer_id);
        put_u64(&mut out, 16, self.next_sequence);
        put_u64(&mut out, 24, self.attempted);
        put_u64(&mut out, 32, self.encoded);
        put_u64(&mut out, 40, self.committed);
        put_u64(&mut out, 48, self.drop_capacity);
        put_u64(&mut out, 56, self.drop_context);
        put_u64(&mut out, 64, self.drop_recursive);
        put_u64(&mut out, 72, self.sink_loss);
        put_u64(&mut out, 80, self.page_count);
        put_u64(&mut out, 88, self.last_page_ordinal);
        put_u32(&mut out, 96, self.thread_generation);
        put_u32(&mut out, 100, self.os_tid);
        put_u64(&mut out, 104, self.flags);
        Ok(out)
    }

    fn validate(&self) -> Result<(), WireError> {
        if self.flags != 0 {
            return Err(WireError::new("ProducerEnd has non-zero v0 flags"));
        }
        let producer_drop = self
            .drop_capacity
            .checked_add(self.drop_context)
            .and_then(|sum| sum.checked_add(self.drop_recursive))
            .ok_or_else(|| WireError::new("ProducerEnd drop total overflow"))?;
        if self.encoded.checked_add(producer_drop) != Some(self.attempted) {
            return Err(WireError::new(
                "ProducerEnd violates attempted = encoded + producer_drop",
            ));
        }
        if self.committed.checked_add(self.sink_loss) != Some(self.encoded) {
            return Err(WireError::new(
                "ProducerEnd violates encoded = committed + sink_loss",
            ));
        }
        if (self.page_count == 0) != (self.last_page_ordinal == u64::MAX) {
            return Err(WireError::new(
                "ProducerEnd page count and last page ordinal disagree",
            ));
        }
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, PRODUCER_END_BYTES, "ProducerEnd")?;
        let control = Control::decode(bytes)?;
        if control.kind != KIND_PRODUCER_END
            || control.version != 0
            || control.flags != 0
            || control.byte_len() != PRODUCER_END_BYTES
        {
            return Err(WireError::new("invalid ProducerEnd control"));
        }
        require_zero(bytes, 112..128, "ProducerEnd reserved bytes")?;
        let end = Self {
            producer_id: read_u64(bytes, 8, "producer id")?,
            next_sequence: read_u64(bytes, 16, "producer next sequence")?,
            attempted: read_u64(bytes, 24, "producer attempted count")?,
            encoded: read_u64(bytes, 32, "producer encoded count")?,
            committed: read_u64(bytes, 40, "producer committed count")?,
            drop_capacity: read_u64(bytes, 48, "capacity drop count")?,
            drop_context: read_u64(bytes, 56, "context drop count")?,
            drop_recursive: read_u64(bytes, 64, "recursive drop count")?,
            sink_loss: read_u64(bytes, 72, "sink loss count")?,
            page_count: read_u64(bytes, 80, "producer page count")?,
            last_page_ordinal: read_u64(bytes, 88, "last page ordinal")?,
            thread_generation: read_u32(bytes, 96, "thread generation")?,
            os_tid: read_u32(bytes, 100, "OS tid")?,
            flags: read_u64(bytes, 104, "ProducerEnd flags")?,
        };
        end.validate()?;
        Ok(end)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionEnd {
    pub(crate) producer_count: u64,
    pub(crate) attempted: u64,
    pub(crate) encoded: u64,
    pub(crate) committed: u64,
    pub(crate) drop_capacity: u64,
    pub(crate) drop_context: u64,
    pub(crate) drop_recursive: u64,
    pub(crate) sink_loss: u64,
    pub(crate) chunks_committed: u64,
    pub(crate) pages_committed: u64,
    pub(crate) bytes_committed: u64,
    pub(crate) write_error_count: u64,
    pub(crate) first_write_errno: i32,
    pub(crate) last_write_errno: i32,
    pub(crate) flags: u64,
}

impl SessionEnd {
    pub(crate) fn to_le_bytes(&self) -> Result<[u8; SESSION_END_BYTES], WireError> {
        self.validate()?;
        let mut out = [0_u8; SESSION_END_BYTES];
        put(
            &mut out,
            0,
            &Control::new(KIND_SESSION_END, 0, SESSION_END_BYTES).to_le_bytes(),
        );
        for (offset, value) in [
            (8, self.producer_count),
            (16, self.attempted),
            (24, self.encoded),
            (32, self.committed),
            (40, self.drop_capacity),
            (48, self.drop_context),
            (56, self.drop_recursive),
            (64, self.sink_loss),
            (72, self.chunks_committed),
            (80, self.pages_committed),
            (88, self.bytes_committed),
            (96, self.write_error_count),
        ] {
            put_u64(&mut out, offset, value);
        }
        put_i32(&mut out, 104, self.first_write_errno);
        put_i32(&mut out, 108, self.last_write_errno);
        put_u64(&mut out, 112, self.flags);
        Ok(out)
    }

    fn validate(&self) -> Result<(), WireError> {
        if self.flags != 0 {
            return Err(WireError::new("SessionEnd has non-zero v0 flags"));
        }
        let producer_drop = self
            .drop_capacity
            .checked_add(self.drop_context)
            .and_then(|sum| sum.checked_add(self.drop_recursive))
            .ok_or_else(|| WireError::new("SessionEnd drop total overflow"))?;
        if self.encoded.checked_add(producer_drop) != Some(self.attempted) {
            return Err(WireError::new(
                "SessionEnd violates attempted = encoded + producer_drop",
            ));
        }
        if self.committed.checked_add(self.sink_loss) != Some(self.encoded) {
            return Err(WireError::new(
                "SessionEnd violates encoded = committed + sink_loss",
            ));
        }
        if (self.write_error_count == 0)
            != (self.first_write_errno == 0 && self.last_write_errno == 0)
        {
            return Err(WireError::new(
                "SessionEnd write error count and errno fields disagree",
            ));
        }
        Ok(())
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        require_exact_len(bytes, SESSION_END_BYTES, "SessionEnd")?;
        let control = Control::decode(bytes)?;
        if control.kind != KIND_SESSION_END
            || control.version != 0
            || control.flags != 0
            || control.byte_len() != SESSION_END_BYTES
        {
            return Err(WireError::new("invalid SessionEnd control"));
        }
        require_zero(bytes, 120..128, "SessionEnd reserved bytes")?;
        let end = Self {
            producer_count: read_u64(bytes, 8, "session producer count")?,
            attempted: read_u64(bytes, 16, "session attempted count")?,
            encoded: read_u64(bytes, 24, "session encoded count")?,
            committed: read_u64(bytes, 32, "session committed count")?,
            drop_capacity: read_u64(bytes, 40, "session capacity drop count")?,
            drop_context: read_u64(bytes, 48, "session context drop count")?,
            drop_recursive: read_u64(bytes, 56, "session recursive drop count")?,
            sink_loss: read_u64(bytes, 64, "session sink loss count")?,
            chunks_committed: read_u64(bytes, 72, "session committed chunks")?,
            pages_committed: read_u64(bytes, 80, "session committed pages")?,
            bytes_committed: read_u64(bytes, 88, "session committed bytes")?,
            write_error_count: read_u64(bytes, 96, "session write error count")?,
            first_write_errno: read_i32(bytes, 104, "first write errno")?,
            last_write_errno: read_i32(bytes, 108, "last write errno")?,
            flags: read_u64(bytes, 112, "SessionEnd flags")?,
        };
        end.validate()?;
        Ok(end)
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
mod tests {
    use super::*;

    #[test]
    fn fixed_record_bytes_are_stable_and_round_trip() {
        let enter = SyscallEnter {
            semantics: SyscallSemantics::Raw,
            nr: 0x0102_0304,
            args: [1, 2, 3, 4, 5, u64::MAX],
        };
        let encoded = enter.to_le_bytes();
        assert_eq!(
            &encoded[..16],
            &[
                0x01, 0x01, 0x00, FLAG_RAW, 0x08, 0x00, 0x00, 0x00, 0x04, 0x03, 0x02, 0x01, 0x00,
                0x00, 0x00, 0x00,
            ]
        );
        assert_eq!(SyscallEnter::decode(&encoded).unwrap(), enter);

        let exit = SyscallExit {
            semantics: SyscallSemantics::Libc,
            result: -1,
            errno: Some(13),
        };
        let encoded = exit.to_le_bytes().unwrap();
        assert_eq!(
            &encoded[..8],
            &[0x02, 0x01, 0x00, FLAG_LIBC, 0x03, 0x00, 0x00, 0x00]
        );
        assert_eq!(&encoded[16..], &[13, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(SyscallExit::decode(&encoded).unwrap(), exit);

        let context = EngineContext {
            engine_id: 0x0102_0304_0506_0708,
        };
        let encoded = context.to_le_bytes();
        assert_eq!(
            &encoded[..8],
            &[
                0x01,
                0x02,
                0x00,
                FLAG_CONTEXT_CONTROL,
                0x02,
                0x00,
                0x00,
                0x00,
            ]
        );
        assert_eq!(EngineContext::decode(&encoded).unwrap(), context);
    }

    #[test]
    fn file_chunk_and_page_headers_round_trip() {
        let file = FileHeader {
            pointer_width: 8,
            clock_kind: CLOCK_NONE,
            pid: 123,
            session_id: [0x5a; 16],
            build_id: 0x0102_0304_0506_0708,
            process_generation: 9,
            segment_number: 0,
            monotonic_anchor: 0,
            wall_unix_ns: 0,
            clock_frequency_num: 0,
            clock_frequency_den: 0,
        };
        let encoded = file.to_le_bytes();
        assert_eq!(&encoded[..8], FILE_MAGIC);
        assert_eq!(FileHeader::decode(&encoded).unwrap(), file);

        let page = PageHeader {
            producer_id: 7,
            first_sequence: 10,
            next_sequence: 12,
            initial_engine_id: 3,
            page_ordinal: 4,
            thread_generation: 5,
            os_tid: 123,
            page_bytes: PAGE_BYTES_4K,
            used_bytes: SYSCALL_PAIR_BYTES as u32,
        };
        let encoded = page.to_le_bytes().unwrap();
        assert_eq!(PageHeader::decode(&encoded).unwrap(), page);

        let chunk = ChunkHeader::new(2, 152, 1, 1).unwrap();
        let encoded_header = chunk.to_le_bytes();
        let payload = [0xa5; 152];
        let footer = ChunkFooter::for_slices(&chunk, &encoded_header, [&payload[..]]).unwrap();
        let encoded_footer = footer.to_le_bytes();
        let decoded_footer = ChunkFooter::decode(&encoded_footer).unwrap();
        decoded_footer
            .verify(&chunk, &encoded_header, &payload)
            .unwrap();
    }

    #[test]
    fn reserved_bits_and_invalid_errno_semantics_fail_loudly() {
        let mut enter = SyscallEnter {
            semantics: SyscallSemantics::Raw,
            nr: 1,
            args: [0; 6],
        }
        .to_le_bytes();
        enter[6] = 1;
        assert!(SyscallEnter::decode(&enter).is_err());

        let raw_errno = SyscallExit {
            semantics: SyscallSemantics::Raw,
            result: -1,
            errno: Some(1),
        };
        assert!(raw_errno.to_le_bytes().is_err());

        let mut success = SyscallExit {
            semantics: SyscallSemantics::Libc,
            result: 4,
            errno: None,
        }
        .to_le_bytes()
        .unwrap();
        success[16] = 2;
        assert!(SyscallExit::decode(&success).is_err());
    }

    #[test]
    fn checksum_detects_header_and_payload_corruption() {
        let file = FileHeader {
            pointer_width: 8,
            clock_kind: CLOCK_NONE,
            pid: 1,
            session_id: [1; 16],
            build_id: 2,
            process_generation: 0,
            segment_number: 0,
            monotonic_anchor: 0,
            wall_unix_ns: 0,
            clock_frequency_num: 0,
            clock_frequency_den: 0,
        };
        let mut encoded = file.to_le_bytes();
        encoded[48] ^= 1;
        assert!(FileHeader::decode(&encoded).is_err());

        let chunk = ChunkHeader::new(0, 8, 1, 0).unwrap();
        let encoded_header = chunk.to_le_bytes();
        let payload = [1_u8; 8];
        let footer = ChunkFooter::for_slices(&chunk, &encoded_header, [&payload[..]]).unwrap();
        let mut corrupt = payload;
        corrupt[3] ^= 1;
        assert!(footer.verify(&chunk, &encoded_header, &corrupt).is_err());
    }
}
