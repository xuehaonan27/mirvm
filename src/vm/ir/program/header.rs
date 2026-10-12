//! Function metadata borrowed independently of executable instructions.

use rkyv::Deserialize;

use super::{ArchivedFuncBody, FuncBody, ParamAbi, RetAbi};

/// An ABI query must not initialize a body's decode or specialization slot. Archived headers
/// borrow the table's validated immutable bytes; copying their scalar fields needs no allocation.
#[derive(Clone, Copy)]
pub(crate) enum FuncHeader<'a> {
    Owned(&'a FuncBody),
    Archived(&'a ArchivedFuncBody),
}

impl<'a> FuncHeader<'a> {
    pub(crate) fn frame_size(self) -> u32 {
        match self {
            Self::Owned(body) => body.frame_size,
            Self::Archived(body) => body.frame_size,
        }
    }

    pub(crate) fn frame_align(self) -> u32 {
        match self {
            Self::Owned(body) => body.frame_align,
            Self::Archived(body) => body.frame_align,
        }
    }

    pub(crate) fn name(self) -> &'a str {
        match self {
            Self::Owned(body) => &body.name,
            Self::Archived(body) => body.name.as_ref(),
        }
    }

    pub(crate) fn ret(self) -> RetAbi {
        match self {
            Self::Owned(body) => body.ret,
            Self::Archived(body) => match body.ret.deserialize(&mut rkyv::Infallible) {
                Ok(ret) => ret,
                Err(never) => match never {},
            },
        }
    }

    pub(crate) fn caller_loc_off(self) -> Option<u32> {
        match self {
            Self::Owned(body) => body.caller_loc_off,
            Self::Archived(body) => body.caller_loc_off.as_ref().copied(),
        }
    }

    pub(crate) fn params(self) -> impl ExactSizeIterator<Item = ParamAbi> + 'a {
        let len = match self {
            Self::Owned(body) => body.params.len(),
            Self::Archived(body) => body.params.len(),
        };
        (0..len).map(move |index| match self {
            Self::Owned(body) => body.params[index],
            Self::Archived(body) => match body.params[index].deserialize(&mut rkyv::Infallible) {
                Ok(param) => param,
                Err(never) => match never {},
            },
        })
    }
}
