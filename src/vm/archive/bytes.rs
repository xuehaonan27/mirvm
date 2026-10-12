//! Immutable file ownership with the alignment required by archived views.

use std::ops::Deref;
use std::sync::Arc;

enum Storage {
    File(Vec<u8>),
    Aligned(rkyv::AlignedVec),
}

pub(crate) struct Bytes(Storage);

impl Bytes {
    pub(crate) fn new(bytes: Vec<u8>) -> Arc<Self> {
        let storage = if bytes.as_ptr().align_offset(rkyv::AlignedVec::ALIGNMENT) == 0 {
            Storage::File(bytes)
        } else {
            // Vec<u8> does not promise archive alignment, even when the allocator usually supplies
            // it. Keep the fast ownership transfer conditional on the actual allocation.
            let mut aligned = rkyv::AlignedVec::with_capacity(bytes.len());
            aligned.extend_from_slice(&bytes);
            Storage::Aligned(aligned)
        };
        Arc::new(Self(storage))
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.0 {
            Storage::File(bytes) => bytes,
            Storage::Aligned(bytes) => bytes,
        }
    }
}
