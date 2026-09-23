//! A record's envelope, and the records that frame a producer's output: the shared eight-byte
//! prefix every record carries, a page header, an engine-context marker, and the two ledgers a
//! producer and a session end with.
//!
//! These are the records about *the capture itself*; [`super::syscall`] holds the record about the
//! guest.

use super::*;

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
