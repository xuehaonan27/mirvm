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

/// What a walk over a body's reference sites does with each one: canonicalization replaces the value
/// with its ordinal, binding replaces the ordinal with the value it names. Sharing one exhaustive
/// walk is what keeps the two directions from drifting.
pub(crate) trait SiteVisitor {
    fn func(&mut self, id: &mut FuncId);
    fn tls(&mut self, id: &mut TlsId);
    fn asm(&mut self, id: &mut AsmStubId);
    fn link(&mut self, addr: &mut LinkAddr);
}

/// Visit every reference site of a body in walk order: blocks in order, statements before the
/// terminator, places and operands in the order the instruction reads them.
pub(crate) fn visit_sites(body: &mut FuncBody, visitor: &mut impl SiteVisitor) {
    for block in &mut body.blocks {
        for stmt in &mut block.stmts {
            visit_stmt(stmt, visitor);
        }
        visit_term(&mut block.term, visitor);
    }
}

/// Canonicalize one body. The clone is what keeps the caller's body (still carrying the ids the
/// runtime needs) untouched.
pub fn canonical(body: &FuncBody) -> Canonical {
    let mut body = body.clone();
    // The symbol name is the manifest's, not the fragment's: it is what names the fragment there.
    body.name = Box::default();
    let mut ordinals = Ordinals::default();
    visit_sites(&mut body, &mut ordinals);
    Canonical {
        body,
        targets: ordinals.targets,
    }
}

/// The fragment's bytes: the encoding version, then the canonical body's postcard form. The
/// version is hashed rather than assumed, so a form change cannot silently alias old fragments.
pub fn encode(body: &FuncBody) -> Result<Vec<u8>, String> {
    encode_canonical(&canonical(body).body)
}

/// [`encode`] for a body that is already canonical, so a caller that projected one does not
/// canonicalize it twice.
pub fn encode_canonical(body: &FuncBody) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    bytes.push(ENCODING_VERSION);
    bytes.extend(postcard::to_stdvec(body).map_err(|e| e.to_string())?);
    Ok(bytes)
}

/// The canonical body inside a fragment's bytes. The encoding version is checked here rather than
/// assumed, so bytes another version wrote are refused instead of misread.
pub fn decode(bytes: &[u8]) -> Result<FuncBody, String> {
    match bytes.split_first() {
        Some((&ENCODING_VERSION, body)) => {
            postcard::from_bytes(body).map_err(|error| format!("fragment decode: {error}"))
        }
        Some((&version, _)) => Err(format!(
            "fragment encoding version {version} is not {ENCODING_VERSION}"
        )),
        None => Err("empty fragment".into()),
    }
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
}

impl SiteVisitor for Ordinals {
    fn func(&mut self, id: &mut FuncId) {
        *id = self.of(Target::Func(*id));
    }
    fn tls(&mut self, id: &mut TlsId) {
        *id = self.of(Target::Tls(*id));
    }
    fn asm(&mut self, id: &mut AsmStubId) {
        *id = self.of(Target::Asm(*id));
    }
    fn link(&mut self, addr: &mut LinkAddr) {
        *addr = LinkAddr(u64::from(self.of(Target::Link(*addr))));
    }
}

fn visit_operand(operand: &mut Operand, visitor: &mut impl SiteVisitor) {
    match operand {
        Operand::Slot(_) | Operand::Imm { .. } => {}
        Operand::Mem { expr, .. } | Operand::AddrOf(expr) => visit_place(expr, visitor),
        Operand::AddrImm(addr) => visitor.link(addr),
        Operand::SubImm { base, .. } => visit_operand(base, visitor),
    }
}

fn visit_place(place: &mut PlaceExpr, visitor: &mut impl SiteVisitor) {
    match &mut place.base {
        PlaceBase::Local(_) => {}
        PlaceBase::Static(addr) => visitor.link(addr),
    }
    // Only the vtable-align step reads a value; the other steps are pure address arithmetic.
    for step in place.steps.iter_mut() {
        if let PlaceStep::VTableAlignOffset { meta, .. } = step {
            visit_operand(meta, visitor);
        }
    }
}

fn visit_scalar_place(place: &mut ScalarPlace, visitor: &mut impl SiteVisitor) {
    if let ScalarPlace::Mem { expr, .. } = place {
        visit_place(expr, visitor);
    }
}

fn visit_ret_dest(ret: &mut RetDest, visitor: &mut impl SiteVisitor) {
    match ret {
        RetDest::Ignore => {}
        RetDest::Scalar(place) => visit_scalar_place(place, visitor),
        RetDest::Pair(a, b) => {
            visit_scalar_place(a, visitor);
            visit_scalar_place(b, visitor);
        }
        RetDest::Indirect(place) => visit_place(place, visitor),
    }
}

fn visit_switch_discr(discr: &mut SwitchDiscr, visitor: &mut impl SiteVisitor) {
    match discr {
        SwitchDiscr::Scalar(operand) => visit_operand(operand, visitor),
        SwitchDiscr::Wide(place) => visit_place(place, visitor),
    }
}

fn visit_bin128_rhs(rhs: &mut Bin128Rhs, visitor: &mut impl SiteVisitor) {
    match rhs {
        Bin128Rhs::Wide(place) => visit_place(place, visitor),
        Bin128Rhs::Scalar(operand) => visit_operand(operand, visitor),
    }
}

fn visit_f128_rhs(rhs: &mut F128Rhs, visitor: &mut impl SiteVisitor) {
    match rhs {
        F128Rhs::Wide(place) => visit_place(place, visitor),
        F128Rhs::Scalar(operand) => visit_operand(operand, visitor),
    }
}

fn visit_rvalue(rvalue: &mut Rvalue, visitor: &mut impl SiteVisitor) {
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
        | Rvalue::AtomicLoad { addr: a, .. } => visit_operand(a, visitor),
        Rvalue::TlsRef(id) => visitor.tls(id),
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
            visit_operand(a, visitor);
            visit_operand(b, visitor);
        }
        Rvalue::NicheDiscr { tag, .. } => visit_operand(tag, visitor),
        Rvalue::MathFma { a, b, c, .. } | Rvalue::MemCmp { a, b, n: c } => {
            visit_operand(a, visitor);
            visit_operand(b, visitor);
            visit_operand(c, visitor);
        }
        Rvalue::Ref(place) => visit_place(place, visitor),
        Rvalue::F128Cmp { a, b, .. } | Rvalue::Cmp128 { a, b, .. } => {
            visit_place(a, visitor);
            visit_place(b, visitor);
        }
        Rvalue::SimdBitmask { a, .. }
        | Rvalue::SimdReduce { a, .. }
        | Rvalue::SimdReduceArith { a, .. } => visit_place(a, visitor),
    }
}

fn visit_stmt(stmt: &mut Stmt, visitor: &mut impl SiteVisitor) {
    match stmt {
        Stmt::Assign { dst, rv } => {
            visit_scalar_place(dst, visitor);
            visit_rvalue(rv, visitor);
        }
        Stmt::AssignOverflow {
            a,
            b,
            dst_val,
            dst_flag,
            ..
        } => {
            visit_operand(a, visitor);
            visit_operand(b, visitor);
            visit_scalar_place(dst_val, visitor);
            visit_scalar_place(dst_flag, visitor);
        }
        Stmt::Copy { dst, src, .. } => {
            visit_place(dst, visitor);
            visit_place(src, visitor);
        }
        Stmt::RepeatScalar { dst, val, .. } => {
            visit_place(dst, visitor);
            visit_operand(val, visitor);
        }
        Stmt::AtomicStore { addr, val, .. } => {
            visit_operand(addr, visitor);
            visit_operand(val, visitor);
        }
        Stmt::VolatileLoad { addr, dst, .. } => {
            visit_operand(addr, visitor);
            visit_place(dst, visitor);
        }
        Stmt::VolatileStore { addr, src, .. } => {
            visit_operand(addr, visitor);
            visit_place(src, visitor);
        }
        Stmt::AtomicCxchg {
            addr,
            expected,
            new,
            dst_val,
            dst_ok,
            ..
        } => {
            visit_operand(addr, visitor);
            visit_operand(expected, visitor);
            visit_operand(new, visitor);
            visit_scalar_place(dst_val, visitor);
            visit_scalar_place(dst_ok, visitor);
        }
        Stmt::AtomicRmw { addr, val, dst, .. } => {
            visit_operand(addr, visitor);
            visit_operand(val, visitor);
            visit_scalar_place(dst, visitor);
        }
        Stmt::MemCopy {
            dst, src, count, ..
        } => {
            visit_operand(dst, visitor);
            visit_operand(src, visitor);
            visit_operand(count, visitor);
        }
        Stmt::MemSet {
            dst, val, count, ..
        } => {
            visit_operand(dst, visitor);
            visit_operand(val, visitor);
            visit_operand(count, visitor);
        }
        Stmt::SimdBin { dst, a, b, .. } => {
            visit_place(dst, visitor);
            visit_place(a, visitor);
            visit_place(b, visitor);
        }
        Stmt::SimdUn { dst, a, .. } => {
            visit_place(dst, visitor);
            visit_place(a, visitor);
        }
        Stmt::SimdFma { dst, a, b, c, .. }
        | Stmt::SimdFunnel {
            dst,
            a,
            b,
            shift: c,
            ..
        } => {
            visit_place(dst, visitor);
            visit_place(a, visitor);
            visit_place(b, visitor);
            visit_place(c, visitor);
        }
        Stmt::SimdCast { dst, src, .. } => {
            visit_place(dst, visitor);
            visit_place(src, visitor);
        }
        Stmt::SimdSelect {
            mask, a, b, dst, ..
        } => {
            visit_place(mask, visitor);
            visit_place(a, visitor);
            visit_place(b, visitor);
            visit_place(dst, visitor);
        }
        Stmt::SimdSelectBitmask {
            mask, a, b, dst, ..
        } => {
            visit_operand(mask, visitor);
            visit_place(a, visitor);
            visit_place(b, visitor);
            visit_place(dst, visitor);
        }
        Stmt::SimdGather {
            passthru,
            ptrs,
            mask,
            dst,
            ..
        } => {
            visit_place(passthru, visitor);
            visit_place(ptrs, visitor);
            visit_place(mask, visitor);
            visit_place(dst, visitor);
        }
        Stmt::SimdScatter {
            values, ptrs, mask, ..
        } => {
            visit_place(values, visitor);
            visit_place(ptrs, visitor);
            visit_place(mask, visitor);
        }
        Stmt::SimdMaskedLoad {
            mask,
            base,
            passthru,
            dst,
            ..
        } => {
            visit_operand(base, visitor);
            visit_place(mask, visitor);
            visit_place(passthru, visitor);
            visit_place(dst, visitor);
        }
        Stmt::SimdMaskedStore {
            mask, base, values, ..
        } => {
            visit_operand(base, visitor);
            visit_place(mask, visitor);
            visit_place(values, visitor);
        }
        Stmt::SimdExtractDyn { src, idx, dst, .. } => {
            visit_place(src, visitor);
            visit_operand(idx, visitor);
            visit_scalar_place(dst, visitor);
        }
        Stmt::SimdInsertDyn {
            src, idx, val, dst, ..
        } => {
            visit_place(src, visitor);
            visit_place(dst, visitor);
            visit_operand(idx, visitor);
            visit_operand(val, visitor);
        }
        Stmt::SimdArithOffset {
            ptrs, offsets, dst, ..
        } => {
            visit_place(ptrs, visitor);
            visit_place(offsets, visitor);
            visit_place(dst, visitor);
        }
        Stmt::SimdSplat { dst, val, .. } => {
            visit_place(dst, visitor);
            visit_operand(val, visitor);
        }
        Stmt::Bin128 { a, b, dst, .. } => {
            visit_place(a, visitor);
            visit_bin128_rhs(b, visitor);
            visit_place(dst, visitor);
        }
        Stmt::Sat128 { a, b, dst, .. } | Stmt::F128Bin { a, b, dst, .. } => {
            visit_place(a, visitor);
            visit_place(b, visitor);
            visit_place(dst, visitor);
        }
        Stmt::F128MathBin { a, b, dst, .. } => {
            visit_place(a, visitor);
            visit_f128_rhs(b, visitor);
            visit_place(dst, visitor);
        }
        Stmt::Wide128ToFloat { src, dst, .. }
        | Stmt::Bit128Count { src, dst, .. }
        | Stmt::F128ToScalar { src, dst, .. } => {
            visit_place(src, visitor);
            visit_scalar_place(dst, visitor);
        }
        Stmt::FloatToWide128 { src, dst, .. } | Stmt::F128FromScalar { src, dst, .. } => {
            visit_operand(src, visitor);
            visit_place(dst, visitor);
        }
        Stmt::Bit128 { src, dst, .. }
        | Stmt::F128Un { a: src, dst, .. }
        | Stmt::F128FromWideInt { src, dst, .. }
        | Stmt::F128ToWideInt { src, dst, .. } => {
            visit_place(src, visitor);
            visit_place(dst, visitor);
        }
        Stmt::F128Fma { a, b, c, dst } => {
            visit_place(a, visitor);
            visit_place(b, visitor);
            visit_place(c, visitor);
            visit_place(dst, visitor);
        }
        Stmt::NicheDiscr128 { tag, dst, .. } => {
            visit_place(tag, visitor);
            visit_scalar_place(dst, visitor);
        }
        Stmt::RepeatBytes { first, .. } => visit_place(first, visitor),
        Stmt::Trap(_) | Stmt::Nop | Stmt::Fence { .. } => {}
    }
}

fn visit_term(term: &mut Terminator, visitor: &mut impl SiteVisitor) {
    match term {
        Terminator::Goto(_) => {}
        Terminator::SwitchInt { discr, .. } => visit_switch_discr(discr, visitor),
        Terminator::Call {
            callee, args, ret, ..
        } => {
            visitor.func(callee);
            for arg in args {
                visit_operand(arg, visitor);
            }
            visit_ret_dest(ret, visitor);
        }
        Terminator::CallBuiltin { args, ret, .. } => {
            for arg in args {
                visit_operand(arg, visitor);
            }
            visit_ret_dest(ret, visitor);
        }
        Terminator::CallForeign { args, ret, .. } => {
            for arg in args {
                visit_operand(arg, visitor);
            }
            visit_ret_dest(ret, visitor);
        }
        Terminator::CallIndirect {
            callee, args, ret, ..
        } => {
            visit_operand(callee, visitor);
            for arg in args {
                visit_operand(arg, visitor);
            }
            visit_ret_dest(ret, visitor);
        }
        Terminator::InlineAsm {
            stub, ins, outs, ..
        } => {
            visitor.asm(stub);
            for (_, value) in ins {
                match value {
                    AsmIoVal::Scalar(operand) => visit_operand(operand, visitor),
                    AsmIoVal::VecBytes(place, _) => visit_place(place, visitor),
                }
            }
            for (_, dst) in outs {
                match dst {
                    AsmIoDst::Scalar(place) => visit_scalar_place(place, visitor),
                    AsmIoDst::VecBytes(place, _) => visit_place(place, visitor),
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
