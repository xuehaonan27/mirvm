//! One aligned, immutable function archive. Structural validation runs before any borrowed view
//! is exposed; verification and demand decoding subsequently use the same owned bytes.

use rkyv::{Archive, Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use crate::error::Error;
use crate::vm::ir::{ArchivedFuncBody, FuncBody, FuncTable, GenericTemplate, SharedBody};

pub(crate) struct Functions {
    bytes: Arc<super::bytes::Bytes>,
    chunks: Vec<Chunk>,
    templates: HashMap<[u8; 32], OnceLock<Arc<GenericTemplate>>>,
}

struct Chunk {
    range: Range<usize>,
    end: usize,
}

impl Functions {
    pub(crate) fn load(
        bytes: Arc<super::bytes::Bytes>,
        range: Range<usize>,
    ) -> Result<Self, Error> {
        let invalid = || crate::fail!(Engine, "invalid function archive chunk table");
        let data = bytes.get(range.clone()).ok_or_else(invalid)?;
        if data.as_ptr().align_offset(rkyv::AlignedVec::ALIGNMENT) != 0 {
            return Err(invalid());
        }
        let word = |at: usize| -> Result<usize, Error> {
            let value = u64::from_le_bytes(
                data.get(at..at.checked_add(8).ok_or_else(invalid)?)
                    .ok_or_else(invalid)?
                    .try_into()
                    .map_err(|_| invalid())?,
            );
            usize::try_from(value).map_err(|_| invalid())
        };
        let count = word(0)?;
        let mut pos = count
            .checked_add(1)
            .and_then(|n| n.checked_mul(8))
            .filter(|&n| n <= data.len())
            .ok_or_else(invalid)?;
        let mut chunks = Vec::with_capacity(count);
        let mut total = 0usize;
        for index in 0..count {
            pos = pos
                .checked_next_multiple_of(rkyv::AlignedVec::ALIGNMENT)
                .ok_or_else(invalid)?;
            let end = pos
                .checked_add(word((index + 1) * 8)?)
                .ok_or_else(invalid)?;
            let chunk = data.get(pos..end).ok_or_else(invalid)?;
            // Each chunk is a self-contained tree. No relative pointer may cross its boundary,
            // allowing its bytes to move intact when an image stack is concatenated or published.
            let mut validator = super::validator::Validator::new(chunk);
            let bodies = rkyv::validation::check_archived_root_with_context::<Vec<FuncBody>, _>(
                chunk,
                &mut validator,
            )
            .map_err(|error| crate::fail!(Engine, format!("invalid function archive: {error}")))?;
            if bodies.is_empty() {
                return Err(invalid());
            }
            total = total
                .checked_add(bodies.len())
                .filter(|&n| n <= u32::MAX as usize)
                .ok_or_else(invalid)?;
            chunks.push(Chunk {
                range: range.start + pos..range.start + end,
                end: total,
            });
            pos = end;
        }
        if pos != data.len() {
            return Err(invalid());
        }
        let mut result = Self {
            bytes,
            chunks,
            templates: HashMap::new(),
        };
        result.templates = result
            .bodies()
            .filter_map(|body| body.shared.as_ref())
            .map(|shared| (shared.template.identity(), OnceLock::new()))
            .collect();
        Ok(result)
    }

    fn chunk_bodies(&self, chunk: &Chunk) -> &[ArchivedFuncBody] {
        // SAFETY: load checked each complete chunk, the aligned allocation never moves or changes,
        // and this borrow cannot outlive it. No filesystem mapping backs this allocation.
        unsafe { rkyv::archived_root::<Vec<FuncBody>>(&self.bytes[chunk.range.clone()]).as_slice() }
    }

    pub(crate) fn bodies(&self) -> impl Iterator<Item = &ArchivedFuncBody> {
        self.chunks
            .iter()
            .flat_map(|chunk| self.chunk_bodies(chunk))
    }

    pub(crate) fn len(&self) -> usize {
        self.chunks.last().map_or(0, |chunk| chunk.end)
    }

    pub(crate) fn body(&self, index: usize) -> &ArchivedFuncBody {
        let chunk_index = self.chunks.partition_point(|chunk| chunk.end <= index);
        let start = chunk_index.checked_sub(1).map_or(0, |i| self.chunks[i].end);
        &self.chunk_bodies(&self.chunks[chunk_index])[index - start]
    }

    pub(crate) fn decode(&self, index: usize) -> FuncBody {
        fn owned<T>(value: &impl Deserialize<T, rkyv::Infallible>) -> T {
            match value.deserialize(&mut rkyv::Infallible) {
                Ok(value) => value,
                Err(never) => match never {},
            }
        }
        let body = self.body(index);
        FuncBody {
            frame_size: body.frame_size,
            frame_align: body.frame_align,
            ret: owned(&body.ret),
            params: owned(&body.params),
            caller_loc_off: owned(&body.caller_loc_off),
            name: owned(&body.name),
            blocks: owned(&body.blocks),
            shared: body.shared.as_ref().map(|shared| {
                let key = shared.template.identity();
                let template =
                    self.templates[&key].get_or_init(|| Arc::new(owned(shared.template.as_ref())));
                Box::new(SharedBody {
                    template: template.clone(),
                    values: owned(&shared.values),
                    locals: owned(&shared.locals),
                    calls: owned(&shared.calls),
                })
            }),
        }
    }
}

/// Copy immutable chunks and encode only owned bodies. The chunk table stores byte lengths;
/// ids inside each chunk retain their absolute positions in the concatenated function table.
pub(crate) fn encode(funcs: &FuncTable) -> Result<rkyv::AlignedVec, Error> {
    enum ChunkBytes<'a> {
        Borrowed(&'a [u8]),
        Owned(rkyv::AlignedVec),
    }
    impl ChunkBytes<'_> {
        fn bytes(&self) -> &[u8] {
            match self {
                Self::Borrowed(bytes) => bytes,
                Self::Owned(bytes) => bytes,
            }
        }
    }
    let mut chunks = Vec::new();
    for part in funcs.parts() {
        match part {
            crate::vm::ir::FuncPart::Owned(bodies) if !bodies.is_empty() => {
                chunks.push(ChunkBytes::Owned(encode_owned(bodies)?));
            }
            crate::vm::ir::FuncPart::Archived(archive) => {
                chunks.extend(
                    archive
                        .chunks
                        .iter()
                        .map(|chunk| ChunkBytes::Borrowed(&archive.bytes[chunk.range.clone()])),
                );
            }
            _ => {}
        }
    }
    if let [ChunkBytes::Owned(bytes)] = chunks.as_mut_slice() {
        // A fresh lowering product already owns the entire payload. Shift it within its allocation
        // to prepend the table; relative pointers stay unchanged and no second archive is needed.
        let len = bytes.len();
        let start = 16usize.next_multiple_of(rkyv::AlignedVec::ALIGNMENT);
        bytes.resize(start + len, 0);
        bytes.as_mut_slice().copy_within(..len, start);
        bytes[..start].fill(0);
        bytes[..8].copy_from_slice(&1u64.to_le_bytes());
        bytes[8..16].copy_from_slice(&(len as u64).to_le_bytes());
        return Ok(std::mem::take(bytes));
    }
    let mut bytes = rkyv::AlignedVec::new();
    bytes.extend_from_slice(&(chunks.len() as u64).to_le_bytes());
    for chunk in &chunks {
        bytes.extend_from_slice(&(chunk.bytes().len() as u64).to_le_bytes());
    }
    for chunk in chunks {
        bytes.resize(bytes.len().next_multiple_of(rkyv::AlignedVec::ALIGNMENT), 0);
        bytes.extend_from_slice(chunk.bytes());
    }
    Ok(bytes)
}

/// Serialize borrowed bodies directly into the Vec archive layout, without cloning the IR.
fn encode_owned(funcs: &[FuncBody]) -> Result<rkyv::AlignedVec, Error> {
    struct Borrowed<'a>(&'a [FuncBody]);

    impl Archive for Borrowed<'_> {
        type Archived = rkyv::vec::ArchivedVec<ArchivedFuncBody>;
        type Resolver = rkyv::vec::VecResolver;

        unsafe fn resolve(&self, pos: usize, resolver: Self::Resolver, out: *mut Self::Archived) {
            // SAFETY: the resolver was produced from exactly these bodies by serialize below.
            unsafe { Self::Archived::resolve_from_len(self.0.len(), pos, resolver, out) };
        }
    }

    impl<S> Serialize<S> for Borrowed<'_>
    where
        S: rkyv::ser::Serializer + rkyv::ser::ScratchSpace + ?Sized,
        FuncBody: Serialize<S>,
    {
        fn serialize(&self, serializer: &mut S) -> Result<Self::Resolver, S::Error> {
            Self::Archived::serialize_from_iter::<FuncBody, _, _, _>(self.0.iter(), serializer)
        }
    }

    rkyv::to_bytes::<_, 4096>(&Borrowed(funcs))
        .map_err(|error| crate::fail!(Engine, format!("cannot encode function archive: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(bytes: &[u8]) -> Result<Functions, Error> {
        Functions::load(
            super::super::bytes::Bytes::new(bytes.to_vec()),
            0..bytes.len(),
        )
    }

    #[test]
    fn generic_identity_unifies_independent_chunks_without_eager_decode() {
        use crate::vm::ir::test_generic_body;
        let mut funcs = FuncTable::default();
        for value in [11, 29] {
            let bytes = encode(&vec![test_generic_body(value)].into()).unwrap();
            funcs.append(FuncTable::from_archive(Arc::new(load(&bytes).unwrap())));
        }
        assert_eq!(funcs.decoded_count(), 0);
        let bytes = encode(&funcs).unwrap();
        assert_eq!(funcs.decoded_count(), 0);
        let combined = load(&bytes).unwrap();
        let a = combined.body(0).shared.as_ref().unwrap();
        let b = combined.body(1).shared.as_ref().unwrap();
        assert!(!std::ptr::eq(a.template.as_ref(), b.template.as_ref()));
        assert_eq!(a.template.identity(), b.template.identity());
        assert_eq!(combined.templates.len(), 1);
        assert!(combined.templates.values().all(|cell| cell.get().is_none()));
        let first = combined.decode(0).shared.unwrap();
        let second = combined.decode(1).shared.unwrap();
        assert!(Arc::ptr_eq(&first.template, &second.template));
        assert_eq!(first.values, [11]);
        assert_eq!(second.values, [29]);
        assert_eq!(first.template.identity(), a.template.identity());
    }

    #[test]
    fn indirect_archive_fields_preserve_runtime_ir() {
        use crate::vm::ir::*;
        let place = PlaceExpr {
            base: PlaceBase::Local(0),
            steps: vec![PlaceStep::VTableAlignOffset {
                meta: Operand::AddrOf(PlaceExpr {
                    base: PlaceBase::Local(8),
                    steps: Box::new([]),
                }),
                // The nested operand exercises both indirect edges of the recursive vocabulary.
                unaligned: 0,
                packed: None,
            }]
            .into_boxed_slice(),
        };
        let sig = ForeignSig {
            args: Vec::new(),
            ret: FfiKind::Void,
            fixed: None,
            thunk_args: Vec::new(),
            unwind: false,
        };
        let body = FuncBody {
            shared: None,
            name: "indirect".into(),
            frame_size: 16,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![
                Block {
                    stmts: vec![Stmt::Assign {
                        dst: ScalarPlace::Mem {
                            expr: place.clone(),
                            width: Width::W64,
                        },
                        rv: Rvalue::Use(Operand::Mem {
                            expr: place.clone(),
                            width: Width::W64,
                        }),
                    }],
                    term: Terminator::CallForeign {
                        sym: "unused".into(),
                        sig: sig.clone(),
                        args: Vec::new(),
                        ret: RetDest::Ignore,
                        target: 1,
                        unwind: UnwindAction::Continue,
                    },
                },
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallIndirect {
                        callee: Operand::AddrOf(place),
                        native_sig: Some(sig),
                        args: Vec::new(),
                        null_ok: false,
                        ret: RetDest::Ignore,
                        target: 2,
                        unwind: UnwindAction::Continue,
                    },
                },
                Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                },
            ],
        };
        let expected = postcard::to_stdvec(&body).unwrap();
        let bytes = encode(&vec![body].into()).unwrap();
        let archive = load(&bytes).unwrap();
        assert_eq!(postcard::to_stdvec(&archive.decode(0)).unwrap(), expected);
    }

    #[test]
    fn rejects_truncation_invalid_discriminants_and_relative_pointers() {
        use crate::vm::ir::*;
        let body = FuncBody {
            shared: None,
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            name: "body".into(),
            blocks: vec![Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            }],
        };
        let encoded = encode(&vec![body].into()).unwrap();
        assert!(load(&encoded[..encoded.len() - 1]).is_err());
        let owner = super::super::bytes::Bytes::new(encoded.to_vec());
        let pointer = owner.as_ptr();
        assert!(Functions::load(owner.clone(), 0..owner.len() + 1).is_err());
        let archive = Functions::load(owner.clone(), 0..owner.len()).unwrap();
        drop(owner);
        assert_eq!(archive.bytes.as_ptr(), pointer);
        assert_eq!(archive.decode(0).name.as_ref(), "body");
        let mut unaligned = vec![0];
        unaligned.extend_from_slice(&encoded);
        let owner = super::super::bytes::Bytes::new(unaligned);
        assert!(Functions::load(owner.clone(), 1..owner.len()).is_err());
        let offset = &archive.bodies().next().unwrap().ret as *const _ as usize
            - archive.bytes.as_ptr() as usize;
        let mut corrupt = encoded.clone();
        corrupt[offset] = 255;
        assert!(load(&corrupt).is_err());
        let mut corrupt = encoded.clone();
        corrupt[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(load(&corrupt).is_err());
        let mut corrupt = encoded.clone();
        corrupt[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(load(&corrupt).is_err());
        let mut corrupt = encoded.clone();
        corrupt.push(0);
        assert!(load(&corrupt).is_err());
        let mut corrupt = encoded;
        let root = corrupt.len() - std::mem::size_of::<rkyv::Archived<Vec<FuncBody>>>();
        corrupt[root..root + 4].copy_from_slice(&i32::MAX.to_ne_bytes());
        assert!(load(&corrupt).is_err());
    }
}
