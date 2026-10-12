//! One aligned, immutable function archive. Structural validation runs before any borrowed view
//! is exposed; verification and demand decoding subsequently use the same owned bytes.

use rkyv::{Archive, Deserialize, Serialize};
use std::ops::Range;
use std::sync::Arc;

use crate::error::Error;
use crate::vm::ir::{ArchivedFuncBody, FuncBody, FuncTable};

pub(crate) struct Functions {
    bytes: Arc<super::bytes::Bytes>,
    range: Range<usize>,
}

impl Functions {
    pub(crate) fn load(
        bytes: Arc<super::bytes::Bytes>,
        range: Range<usize>,
    ) -> Result<Self, Error> {
        let data = bytes.get(range.clone()).ok_or_else(|| {
            crate::fail!(
                Engine,
                "function archive range is outside its immutable owner"
            )
        })?;
        // There are no shared pointers in bytecode. A bounded tree validator rejects cycles,
        // aliasing, out-of-bounds pointers and excessive nesting before the semantic walk.
        let mut validator =
            rkyv::validation::validators::ArchiveValidator::with_max_depth(data, 128);
        rkyv::validation::check_archived_root_with_context::<Vec<FuncBody>, _>(
            data,
            &mut validator,
        )
        .map_err(|error| crate::fail!(Engine, format!("invalid function archive: {error}")))?;
        Ok(Self { bytes, range })
    }

    pub(crate) fn bodies(&self) -> &[ArchivedFuncBody] {
        // SAFETY: load checked the entire archive, the aligned allocation never moves or changes,
        // and this borrow cannot outlive it. No filesystem mapping backs this allocation.
        unsafe { rkyv::archived_root::<Vec<FuncBody>>(&self.bytes[self.range.clone()]).as_slice() }
    }

    pub(crate) fn len(&self) -> usize {
        self.bodies().len()
    }

    pub(crate) fn decode(&self, index: usize) -> FuncBody {
        match self.bodies()[index].deserialize(&mut rkyv::Infallible) {
            Ok(body) => body,
            Err(never) => match never {},
        }
    }
}

/// Serialize borrowed bodies directly into the Vec archive layout, without cloning the IR.
pub(crate) fn encode(funcs: &FuncTable) -> Result<rkyv::AlignedVec, Error> {
    struct Borrowed<'a>(&'a FuncTable);

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
        let offset =
            &archive.bodies()[0].ret as *const _ as usize - archive.bytes.as_ptr() as usize;
        let mut corrupt = encoded.clone();
        corrupt[offset] = 255;
        assert!(load(&corrupt).is_err());
        let mut corrupt = encoded;
        let root = corrupt.len() - std::mem::size_of::<rkyv::Archived<Vec<FuncBody>>>();
        corrupt[root..root + 4].copy_from_slice(&i32::MAX.to_ne_bytes());
        assert!(load(&corrupt).is_err());
    }
}
