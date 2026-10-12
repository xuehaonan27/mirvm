//! The semantic walk compiled against borrowed archive views. Keeping one walk for both forms
//! makes new instructions and validation rules apply equally to lowered and stored bytecode.

use super::{REGION_CAP, buffer_span, vector};
use crate::vm::ir::{
    ArchivedAsmIoDst as AsmIoDst, ArchivedAsmIoVal as AsmIoVal, ArchivedBin128Rhs as Bin128Rhs,
    ArchivedBuiltin as Builtin, ArchivedBuiltinCallRole as BuiltinCallRole,
    ArchivedCallRole as CallRole, ArchivedF128Rhs as F128Rhs, ArchivedFfiAgg as FfiAgg,
    ArchivedFfiKind as FfiKind, ArchivedFfiLeaf as FfiLeaf, ArchivedForeignSig as ForeignSig,
    ArchivedFuncBody as FuncBody, ArchivedOperand as Operand, ArchivedParamAbi as ParamAbi,
    ArchivedPlaceBase as PlaceBase, ArchivedPlaceExpr as PlaceExpr, ArchivedPlaceStep as PlaceStep,
    ArchivedRetAbi as RetAbi, ArchivedRetDest as RetDest, ArchivedRvalue as Rvalue,
    ArchivedScalarPlace as ScalarPlace, ArchivedSlot as Slot, ArchivedStmt as Stmt,
    ArchivedSwitchDiscr as SwitchDiscr, ArchivedTerminator as Terminator,
    ArchivedUnwindAction as UnwindAction, ArchivedWidth as Width, AsmStubId, Bb, FuncId, TlsId,
};

struct Verifier<'a>(super::Verifier<'a>);

impl<'a> std::ops::Deref for Verifier<'a> {
    type Target = super::Verifier<'a>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

mod shared {
    include!("shared.rs");
}

mod body {
    include!("body.rs");
}
mod ffi {
    include!("ffi.rs");
}
mod vocab {
    include!("vocab.rs");
}

impl Width {
    fn bytes(self) -> u32 {
        match self {
            Self::W8 => 1,
            Self::W16 => 2,
            Self::W32 => 4,
            Self::W64 => 8,
        }
    }
}

impl Operand {
    fn width(&self) -> Width {
        match self {
            Self::Slot(slot) => slot.width,
            Self::Mem { width, .. } | Self::Imm { width, .. } => *width,
            Self::AddrImm(_) | Self::AddrOf(_) => Width::W64,
            Self::SubImm { base, .. } => base.width(),
        }
    }
}

pub(crate) fn function(
    module: &super::Module,
    instance: &super::Instance,
    funcs: usize,
    index: usize,
    body: &FuncBody,
    addresses: &std::cell::RefCell<super::AddressUses>,
) -> Result<(usize, usize), crate::error::Error> {
    let mut context =
        super::Verifier::new_with_count(module, instance, super::Prefix::default(), &[], funcs)?;
    context.address_uses = Some(addresses);
    function_in(context, index, body)
}

pub(super) fn function_in(
    context: super::Verifier<'_>,
    index: usize,
    body: &FuncBody,
) -> Result<(usize, usize), crate::error::Error> {
    let verifier = Verifier(context);
    verifier.body(body).map_err(|error| {
        crate::fail!(Engine, format!("function {index} `{}`: {error}", body.name))
    })?;
    Ok(body
        .code()
        .iter()
        .fold((0, 0), |(boundaries, catchers), block| {
            match body.term(block) {
                Terminator::Call {
                    role: CallRole::MainPanicBoundary,
                    ..
                } => (boundaries + 1, catchers),
                Terminator::CallBuiltin {
                    role: BuiltinCallRole::MainPanicCatcher,
                    ..
                } => (boundaries, catchers + 1),
                _ => (boundaries, catchers),
            }
        }))
}

fn ffi_index(index: u32) -> usize {
    index as usize
}
