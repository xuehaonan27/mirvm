//! The canonical at-rest form of one lowered function body, and the fragment id taken over it.
//!
//! A body can only be shared across crate versions, feature sets and projects if it is written down
//! without the things those differ in. Two of them are in the body: the symbol name stays with the
//! manifest, and every reference site becomes a dense ordinal in order of first appearance. The
//! numbers lowering assigned (function, TLS and asm-stub ids) and the addresses it baked
//! (frozen-region offsets) therefore leave the bytes and move into the manifest's binding table,
//! where `targets[ordinal]` names what that ordinal stood for. Two bodies that lower identically
//! share one fragment id whatever stack they were lowered above.
//!
//! The reference sites are exactly the ones [`crate::lower::rebase`] walks: `Call.callee`,
//! `InlineAsm.stub` and `Rvalue::TlsRef` carry ids, `PlaceBase::Static` and `Operand::AddrImm` carry
//! addresses. The matches below are exhaustive on purpose — a new statement, rvalue or terminator
//! variant must fail to compile here rather than silently keep a lowering-assigned number in the
//! canonical bytes.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use super::*;

/// First byte of the encoded fragment: a change to the canonical form itself (a new reference site,
/// a different ordinal rule) must invalidate every fragment id, and hashing the version is what
/// makes that automatic instead of a remembered migration.
pub const ENCODING_VERSION: u8 = 1;

/// What one ordinal of a canonical body stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    Func(FuncId),
    Tls(TlsId),
    Asm(AsmStubId),
    Link(LinkAddr),
}

/// A body in canonical form plus, in ordinal order, the target each ordinal names.
pub struct Canonical {
    pub body: FuncBody,
    /// `targets[ordinal]` is what that ordinal stood for in the lowered body.
    pub targets: Vec<Target>,
}

/// Canonicalize one body. The clone is what keeps the caller's body (still carrying the ids the
/// runtime needs) untouched.
pub fn canonical(body: &FuncBody) -> Canonical {
    let mut body = body.clone();
    // The symbol name is the manifest's, not the fragment's: it is what names the fragment there.
    body.name = Box::default();
    let mut ordinals = Ordinals::default();
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            rewrite_stmt(stmt, &mut ordinals);
        }
        rewrite_term(&mut block.term, &mut ordinals);
    }
    Canonical {
        body,
        targets: ordinals.targets,
    }
}

/// The fragment's bytes: the encoding version, then the canonical body's postcard form. The
/// version is hashed rather than assumed, so a form change cannot silently alias old fragments.
pub fn encode(body: &FuncBody) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    bytes.push(ENCODING_VERSION);
    bytes.extend(postcard::to_stdvec(&canonical(body).body).map_err(|e| e.to_string())?);
    Ok(bytes)
}

/// The content address of already-encoded fragment bytes.
pub fn id_of(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// The content address of one body.
pub fn id(body: &FuncBody) -> Result<[u8; 32], String> {
    Ok(id_of(&encode(body)?))
}

/// Dense ordinal assignment. One namespace for all five sites, because a binding is one tagged
/// value: a function id and a link address that happen to be numerically equal are still two
/// targets, and a body that names one target twice reuses one ordinal.
#[derive(Default)]
struct Ordinals {
    seen: HashMap<Target, u32>,
    targets: Vec<Target>,
}

impl Ordinals {
    fn of(&mut self, target: Target) -> u32 {
        match self.seen.entry(target) {
            Entry::Occupied(seen) => *seen.get(),
            Entry::Vacant(slot) => {
                let ordinal = self.targets.len() as u32;
                self.targets.push(target);
                slot.insert(ordinal);
                ordinal
            }
        }
    }

    /// Rewrite a link address in place to its ordinal.
    fn link(&mut self, addr: &mut LinkAddr) {
        *addr = LinkAddr(u64::from(self.of(Target::Link(*addr))));
    }
}

fn rewrite_operand(operand: &mut Operand, ord: &mut Ordinals) {
    match operand {
        Operand::Slot(_) | Operand::Imm { .. } => {}
        Operand::Mem { expr, .. } | Operand::AddrOf(expr) => rewrite_place(expr, ord),
        Operand::AddrImm(addr) => ord.link(addr),
        Operand::SubImm { base, .. } => rewrite_operand(base, ord),
    }
}

fn rewrite_place(place: &mut PlaceExpr, ord: &mut Ordinals) {
    match &mut place.base {
        PlaceBase::Local(_) => {}
        PlaceBase::Static(addr) => ord.link(addr),
    }
    // Only the vtable-align step reads a value; the other steps are pure address arithmetic.
    for step in place.steps.iter_mut() {
        if let PlaceStep::VTableAlignOffset { meta, .. } = step {
            rewrite_operand(meta, ord);
        }
    }
}

fn rewrite_scalar_place(place: &mut ScalarPlace, ord: &mut Ordinals) {
    if let ScalarPlace::Mem { expr, .. } = place {
        rewrite_place(expr, ord);
    }
}

fn rewrite_ret_dest(ret: &mut RetDest, ord: &mut Ordinals) {
    match ret {
        RetDest::Ignore => {}
        RetDest::Scalar(place) => rewrite_scalar_place(place, ord),
        RetDest::Pair(a, b) => {
            rewrite_scalar_place(a, ord);
            rewrite_scalar_place(b, ord);
        }
        RetDest::Indirect(place) => rewrite_place(place, ord),
    }
}

fn rewrite_switch_discr(discr: &mut SwitchDiscr, ord: &mut Ordinals) {
    match discr {
        SwitchDiscr::Scalar(operand) => rewrite_operand(operand, ord),
        SwitchDiscr::Wide(place) => rewrite_place(place, ord),
    }
}

fn rewrite_bin128_rhs(rhs: &mut Bin128Rhs, ord: &mut Ordinals) {
    match rhs {
        Bin128Rhs::Wide(place) => rewrite_place(place, ord),
        Bin128Rhs::Scalar(operand) => rewrite_operand(operand, ord),
    }
}

fn rewrite_f128_rhs(rhs: &mut F128Rhs, ord: &mut Ordinals) {
    match rhs {
        F128Rhs::Wide(place) => rewrite_place(place, ord),
        F128Rhs::Scalar(operand) => rewrite_operand(operand, ord),
    }
}

fn rewrite_rvalue(rvalue: &mut Rvalue, ord: &mut Ordinals) {
    match rvalue {
        Rvalue::Use(a)
        | Rvalue::NotBits(a)
        | Rvalue::NotBool(a)
        | Rvalue::Neg(a)
        | Rvalue::Cast { a, .. }
        | Rvalue::MathUn { a, .. }
        | Rvalue::FloatNeg { a, .. }
        | Rvalue::FloatCast { a, .. }
        | Rvalue::FloatToInt { a, .. }
        | Rvalue::IntToFloat { a, .. }
        | Rvalue::BitUn { a, .. }
        | Rvalue::AtomicLoad { addr: a, .. } => rewrite_operand(a, ord),
        Rvalue::TlsRef(id) => *id = ord.of(Target::Tls(*id)),
        Rvalue::IntBin { a, b, .. }
        | Rvalue::IntCmp { a, b, .. }
        | Rvalue::PtrOffset {
            ptr: a, count: b, ..
        }
        | Rvalue::IntCmp3 { a, b, .. }
        | Rvalue::FloatBin { a, b, .. }
        | Rvalue::MathBin { a, b, .. }
        | Rvalue::UMax { a, b }
        | Rvalue::FloatCmp { a, b, .. }
        | Rvalue::PtrDiff { a, b, .. }
        | Rvalue::IntSat { a, b, .. } => {
            rewrite_operand(a, ord);
            rewrite_operand(b, ord);
        }
        Rvalue::NicheDiscr { tag, .. } => rewrite_operand(tag, ord),
        Rvalue::MathFma { a, b, c, .. } | Rvalue::MemCmp { a, b, n: c } => {
            rewrite_operand(a, ord);
            rewrite_operand(b, ord);
            rewrite_operand(c, ord);
        }
        Rvalue::Ref(place) => rewrite_place(place, ord),
        Rvalue::F128Cmp { a, b, .. } | Rvalue::Cmp128 { a, b, .. } => {
            rewrite_place(a, ord);
            rewrite_place(b, ord);
        }
        Rvalue::SimdBitmask { a, .. }
        | Rvalue::SimdReduce { a, .. }
        | Rvalue::SimdReduceArith { a, .. } => rewrite_place(a, ord),
    }
}

fn rewrite_stmt(stmt: &mut Stmt, ord: &mut Ordinals) {
    match stmt {
        Stmt::Assign { dst, rv } => {
            rewrite_scalar_place(dst, ord);
            rewrite_rvalue(rv, ord);
        }
        Stmt::AssignOverflow {
            a,
            b,
            dst_val,
            dst_flag,
            ..
        } => {
            rewrite_operand(a, ord);
            rewrite_operand(b, ord);
            rewrite_scalar_place(dst_val, ord);
            rewrite_scalar_place(dst_flag, ord);
        }
        Stmt::Copy { dst, src, .. } => {
            rewrite_place(dst, ord);
            rewrite_place(src, ord);
        }
        Stmt::RepeatScalar { dst, val, .. } => {
            rewrite_place(dst, ord);
            rewrite_operand(val, ord);
        }
        Stmt::AtomicStore { addr, val, .. } => {
            rewrite_operand(addr, ord);
            rewrite_operand(val, ord);
        }
        Stmt::VolatileLoad { addr, dst, .. } => {
            rewrite_operand(addr, ord);
            rewrite_place(dst, ord);
        }
        Stmt::VolatileStore { addr, src, .. } => {
            rewrite_operand(addr, ord);
            rewrite_place(src, ord);
        }
        Stmt::AtomicCxchg {
            addr,
            expected,
            new,
            dst_val,
            dst_ok,
            ..
        } => {
            rewrite_operand(addr, ord);
            rewrite_operand(expected, ord);
            rewrite_operand(new, ord);
            rewrite_scalar_place(dst_val, ord);
            rewrite_scalar_place(dst_ok, ord);
        }
        Stmt::AtomicRmw { addr, val, dst, .. } => {
            rewrite_operand(addr, ord);
            rewrite_operand(val, ord);
            rewrite_scalar_place(dst, ord);
        }
        Stmt::MemCopy {
            dst, src, count, ..
        } => {
            rewrite_operand(dst, ord);
            rewrite_operand(src, ord);
            rewrite_operand(count, ord);
        }
        Stmt::MemSet {
            dst, val, count, ..
        } => {
            rewrite_operand(dst, ord);
            rewrite_operand(val, ord);
            rewrite_operand(count, ord);
        }
        Stmt::SimdBin { dst, a, b, .. } => {
            rewrite_place(dst, ord);
            rewrite_place(a, ord);
            rewrite_place(b, ord);
        }
        Stmt::SimdUn { dst, a, .. } => {
            rewrite_place(dst, ord);
            rewrite_place(a, ord);
        }
        Stmt::SimdFma { dst, a, b, c, .. }
        | Stmt::SimdFunnel {
            dst,
            a,
            b,
            shift: c,
            ..
        } => {
            rewrite_place(dst, ord);
            rewrite_place(a, ord);
            rewrite_place(b, ord);
            rewrite_place(c, ord);
        }
        Stmt::SimdCast { dst, src, .. } => {
            rewrite_place(dst, ord);
            rewrite_place(src, ord);
        }
        Stmt::SimdSelect {
            mask, a, b, dst, ..
        } => {
            rewrite_place(mask, ord);
            rewrite_place(a, ord);
            rewrite_place(b, ord);
            rewrite_place(dst, ord);
        }
        Stmt::SimdSelectBitmask {
            mask, a, b, dst, ..
        } => {
            rewrite_operand(mask, ord);
            rewrite_place(a, ord);
            rewrite_place(b, ord);
            rewrite_place(dst, ord);
        }
        Stmt::SimdGather {
            passthru,
            ptrs,
            mask,
            dst,
            ..
        } => {
            rewrite_place(passthru, ord);
            rewrite_place(ptrs, ord);
            rewrite_place(mask, ord);
            rewrite_place(dst, ord);
        }
        Stmt::SimdScatter {
            values, ptrs, mask, ..
        } => {
            rewrite_place(values, ord);
            rewrite_place(ptrs, ord);
            rewrite_place(mask, ord);
        }
        Stmt::SimdMaskedLoad {
            mask,
            base,
            passthru,
            dst,
            ..
        } => {
            rewrite_operand(base, ord);
            rewrite_place(mask, ord);
            rewrite_place(passthru, ord);
            rewrite_place(dst, ord);
        }
        Stmt::SimdMaskedStore {
            mask, base, values, ..
        } => {
            rewrite_operand(base, ord);
            rewrite_place(mask, ord);
            rewrite_place(values, ord);
        }
        Stmt::SimdExtractDyn { src, idx, dst, .. } => {
            rewrite_place(src, ord);
            rewrite_operand(idx, ord);
            rewrite_scalar_place(dst, ord);
        }
        Stmt::SimdInsertDyn {
            src, idx, val, dst, ..
        } => {
            rewrite_place(src, ord);
            rewrite_place(dst, ord);
            rewrite_operand(idx, ord);
            rewrite_operand(val, ord);
        }
        Stmt::SimdArithOffset {
            ptrs, offsets, dst, ..
        } => {
            rewrite_place(ptrs, ord);
            rewrite_place(offsets, ord);
            rewrite_place(dst, ord);
        }
        Stmt::SimdSplat { dst, val, .. } => {
            rewrite_place(dst, ord);
            rewrite_operand(val, ord);
        }
        Stmt::Bin128 { a, b, dst, .. } => {
            rewrite_place(a, ord);
            rewrite_bin128_rhs(b, ord);
            rewrite_place(dst, ord);
        }
        Stmt::Sat128 { a, b, dst, .. } | Stmt::F128Bin { a, b, dst, .. } => {
            rewrite_place(a, ord);
            rewrite_place(b, ord);
            rewrite_place(dst, ord);
        }
        Stmt::F128MathBin { a, b, dst, .. } => {
            rewrite_place(a, ord);
            rewrite_f128_rhs(b, ord);
            rewrite_place(dst, ord);
        }
        Stmt::Wide128ToFloat { src, dst, .. }
        | Stmt::Bit128Count { src, dst, .. }
        | Stmt::F128ToScalar { src, dst, .. } => {
            rewrite_place(src, ord);
            rewrite_scalar_place(dst, ord);
        }
        Stmt::FloatToWide128 { src, dst, .. } | Stmt::F128FromScalar { src, dst, .. } => {
            rewrite_operand(src, ord);
            rewrite_place(dst, ord);
        }
        Stmt::Bit128 { src, dst, .. }
        | Stmt::F128Un { a: src, dst, .. }
        | Stmt::F128FromWideInt { src, dst, .. }
        | Stmt::F128ToWideInt { src, dst, .. } => {
            rewrite_place(src, ord);
            rewrite_place(dst, ord);
        }
        Stmt::F128Fma { a, b, c, dst } => {
            rewrite_place(a, ord);
            rewrite_place(b, ord);
            rewrite_place(c, ord);
            rewrite_place(dst, ord);
        }
        Stmt::NicheDiscr128 { tag, dst, .. } => {
            rewrite_place(tag, ord);
            rewrite_scalar_place(dst, ord);
        }
        Stmt::RepeatBytes { first, .. } => rewrite_place(first, ord),
        Stmt::Trap(_) | Stmt::Nop | Stmt::Fence { .. } => {}
    }
}

fn rewrite_term(term: &mut Terminator, ord: &mut Ordinals) {
    match term {
        Terminator::Goto(_) => {}
        Terminator::SwitchInt { discr, .. } => rewrite_switch_discr(discr, ord),
        Terminator::Call {
            callee, args, ret, ..
        } => {
            *callee = ord.of(Target::Func(*callee));
            for arg in args {
                rewrite_operand(arg, ord);
            }
            rewrite_ret_dest(ret, ord);
        }
        Terminator::CallBuiltin { args, ret, .. } => {
            for arg in args {
                rewrite_operand(arg, ord);
            }
            rewrite_ret_dest(ret, ord);
        }
        Terminator::CallForeign { args, ret, .. } => {
            for arg in args {
                rewrite_operand(arg, ord);
            }
            rewrite_ret_dest(ret, ord);
        }
        Terminator::CallIndirect {
            callee, args, ret, ..
        } => {
            rewrite_operand(callee, ord);
            for arg in args {
                rewrite_operand(arg, ord);
            }
            rewrite_ret_dest(ret, ord);
        }
        Terminator::InlineAsm {
            stub, ins, outs, ..
        } => {
            *stub = ord.of(Target::Asm(*stub));
            for (_, value) in ins {
                match value {
                    AsmIoVal::Scalar(operand) => rewrite_operand(operand, ord),
                    AsmIoVal::VecBytes(place, _) => rewrite_place(place, ord),
                }
            }
            for (_, dst) in outs {
                match dst {
                    AsmIoDst::Scalar(place) => rewrite_scalar_place(place, ord),
                    AsmIoDst::VecBytes(place, _) => rewrite_place(place, ord),
                }
            }
        }
        Terminator::Return
        | Terminator::Unreachable
        | Terminator::Resume
        | Terminator::TerminateAbort
        | Terminator::Trap(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_with(term: Terminator) -> FuncBody {
        FuncBody {
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: Vec::new(),
                term,
            }],
            name: "sym".into(),
        }
    }

    fn slot(off: u32) -> ScalarPlace {
        ScalarPlace::Slot(Slot {
            off,
            width: Width::W64,
        })
    }

    fn static_operand(addr: u64) -> Operand {
        Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Static(LinkAddr(addr)),
                steps: Box::new([]),
            },
            width: Width::W64,
        }
    }

    fn call(callee: FuncId, arg: Operand) -> FuncBody {
        body_with(Terminator::Call {
            callee,
            args: vec![arg],
            ret: RetDest::Ignore,
            target: 0,
            unwind: UnwindAction::Continue,
            role: CallRole::Normal,
        })
    }

    fn with_tls(mut body: FuncBody, id: TlsId) -> FuncBody {
        body.blocks[0].stmts.push(Stmt::Assign {
            dst: slot(8),
            rv: Rvalue::TlsRef(id),
        });
        body
    }

    #[test]
    fn lowering_assigned_numbers_and_baked_addresses_leave_the_bytes() {
        // The two things a crate version or a stack changes: which ids lowering handed out and where
        // the frozen region landed. Neither may reach the fragment id.
        let a = call(7, static_operand(0x6a00_0000_1000));
        let b = call(91, static_operand(0x6a00_0000_4400));
        assert_eq!(id(&a).unwrap(), id(&b).unwrap());
    }

    #[test]
    fn the_symbol_name_is_the_manifests_not_the_fragments() {
        let mut a = body_with(Terminator::Return);
        let mut b = body_with(Terminator::Return);
        b.name = "another::symbol".into();
        assert_eq!(id(&a).unwrap(), id(&b).unwrap());
        a.frame_size = 16;
        assert_ne!(id(&a).unwrap(), id(&b).unwrap());
    }

    #[test]
    fn ordinals_follow_first_appearance_and_share_one_namespace() {
        // Call(7), a TLS reference, then Call(7) again in a second block: three sites, two ordinals,
        // and the second call reuses the first call's ordinal.
        let mut body = with_tls(
            call(
                7,
                Operand::Slot(Slot {
                    off: 0,
                    width: Width::W64,
                }),
            ),
            3,
        );
        body.blocks.push(body.blocks[0].clone());

        let canonical = canonical(&body);
        assert_eq!(
            canonical.targets,
            vec![Target::Tls(3), Target::Func(7)],
            "one namespace, assigned in walk order (statements before the terminator)"
        );
        let Terminator::Call { callee, .. } = canonical.body.blocks[0].term else {
            unreachable!()
        };
        assert_eq!(callee, 1);
        let Stmt::Assign { rv, .. } = &canonical.body.blocks[0].stmts[0] else {
            unreachable!()
        };
        assert!(matches!(rv, Rvalue::TlsRef(0)));
        let Terminator::Call { callee, .. } = canonical.body.blocks[1].term else {
            unreachable!()
        };
        assert_eq!(callee, 1, "the same target reuses its ordinal");
    }

    #[test]
    fn a_body_that_only_differs_in_which_target_it_names_shares_the_fragment() {
        // The binding table is what tells the two apart: the same code naming a different TLS slot
        // is one fragment reached through two bindings. This is the sharing the design exists for.
        let a = with_tls(
            call(
                7,
                Operand::Slot(Slot {
                    off: 0,
                    width: Width::W64,
                }),
            ),
            3,
        );
        let b = with_tls(
            call(
                7,
                Operand::Slot(Slot {
                    off: 0,
                    width: Width::W64,
                }),
            ),
            9,
        );
        assert_eq!(id(&a).unwrap(), id(&b).unwrap());
        assert_eq!(canonical(&a).targets, vec![Target::Tls(3), Target::Func(7)]);
        assert_eq!(canonical(&b).targets, vec![Target::Tls(9), Target::Func(7)]);
    }

    #[test]
    fn a_different_instruction_sequence_is_a_different_fragment() {
        // Same targets, different code: the fragment id is the body, not the symbols it mentions.
        let a = call(
            7,
            Operand::Slot(Slot {
                off: 0,
                width: Width::W64,
            }),
        );
        let b = body_with(Terminator::Call {
            callee: 7,
            args: vec![
                Operand::Slot(Slot {
                    off: 0,
                    width: Width::W64,
                }),
                Operand::Slot(Slot {
                    off: 8,
                    width: Width::W64,
                }),
            ],
            ret: RetDest::Ignore,
            target: 0,
            unwind: UnwindAction::Continue,
            role: CallRole::Normal,
        });
        assert_ne!(id(&a).unwrap(), id(&b).unwrap());
    }

    #[test]
    fn distinct_target_kinds_never_share_an_ordinal() {
        // A function id and a link address that happen to be numerically equal are different
        // bindings, so they must not collapse into one ordinal.
        let body = call(3, Operand::AddrImm(LinkAddr(3)));
        let canonical = canonical(&body);
        assert_eq!(
            canonical.targets,
            vec![Target::Func(3), Target::Link(LinkAddr(3))]
        );
        let Terminator::Call { args, .. } = &canonical.body.blocks[0].term else {
            unreachable!()
        };
        assert!(matches!(args[0], Operand::AddrImm(LinkAddr(1))));
    }

    #[test]
    fn the_encoding_version_is_the_first_byte() {
        let bytes = encode(&body_with(Terminator::Return)).unwrap();
        assert_eq!(bytes[0], ENCODING_VERSION);
        assert_eq!(id_of(&bytes), id(&body_with(Terminator::Return)).unwrap());
    }
}
