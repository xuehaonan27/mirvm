//! Engine bytecode IR: typed, pure Rust, with no `rustc` types anywhere, so a compiled Module is a
//! fully self-contained artifact.
//!
//! Addresses are real runtime addresses, so a static offset is not enough to name a memory
//! location: deref and index projections produce a `PlaceExpr` that the engine evaluates in order
//! to obtain the address. The frame base is a real address too, which makes frame-local, heap and
//! static accesses all plain raw-address reads and writes.
//! Purely frame-local scalar accesses with a static offset keep the `Slot` fast path, which needs no
//! evaluation at all.
//!
//! This IR is the real engine body. The instruction families and the module tables they
//! populate live in `program`; this module owns the operand/width/operation vocabulary they
//! are built from.

mod program;

// The instruction families and module tables live in the child module. Re-export them so every
// `crate::vm::ir::X` path a caller already uses keeps resolving.
pub use program::*;

pub type Bb = u32;
pub type FuncId = u32;
/// Inline asm site id: index into `Module.asm_stub_addrs`.
pub type AsmStubId = u32;

/// One asm-stub materialization recipe. Symbol names are decoupled from bit order; see
/// `Module.asm_sites`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AsmSite {
    /// dlsym symbol name of the wrapper (baked into `.globl`/`.type`/`.size` when lower emits GAS text)
    pub name: Box<str>,
    /// Full wrapper GAS text
    pub text: String,
}

/// Scalar width. W128 is carried by the wide two-slot channel, not by this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Width {
    W8,
    W16,
    W32,
    W64,
}

impl Width {
    #[inline]
    pub fn bytes(self) -> u32 {
        match self {
            Width::W8 => 1,
            Width::W16 => 2,
            Width::W32 => 4,
            Width::W64 => 8,
        }
    }
    #[inline]
    pub fn mask(self) -> u64 {
        match self {
            Width::W8 => 0xff,
            Width::W16 => 0xffff,
            Width::W32 => 0xffff_ffff,
            Width::W64 => u64::MAX,
        }
    }
    pub fn from_bytes(n: u64) -> Option<Width> {
        Some(match n {
            1 => Width::W8,
            2 => Width::W16,
            4 => Width::W32,
            8 => Width::W64,
            _ => return None,
        })
    }
}

/// InlineAsm input value channel.
/// `Scalar` is an 8-byte value written low in its slot; `VecBytes` is a vector byte channel that
/// names a real address and the full width (xmm/ymm/zmm = 16/32/64 bytes), the same source the
/// wrapper reads with `movups`/`vmovups`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum AsmIoVal {
    Scalar(Operand),
    VecBytes(PlaceExpr, u32),
}

/// InlineAsm output destination channel, the dual of `AsmIoVal`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum AsmIoDst {
    Scalar(ScalarPlace),
    VecBytes(PlaceExpr, u32),
}

/// Frame-local scalar slot (fast path): the offset is frozen, with any Field projections already
/// folded into it.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Slot {
    pub off: u32,
    pub width: Width,
}

// ===== Place evaluation =====

/// Base of an address expression.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum PlaceBase {
    /// Frame-local: real address = frame base + off
    Local(u32),
    /// Frozen-area real address (statics and the constant pool)
    Static(LinkAddr),
}

/// Link-time address recorded in the package. Distinct from ordinary integers so a loaded instance can
/// uniformly translate it to the instance address.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LinkAddr(pub u64);

impl std::fmt::LowerHex for LinkAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::LowerHex::fmt(&self.0, f)
    }
}

#[derive(Clone, Copy, Debug)]
struct LoadRange {
    link_start: u64,
    runtime_start: u64,
    len: u64,
}

/// Per-Module-instance address translation table. Artifacts store `LinkAddr`; the runtime address is
/// only known after instance creation. Ordinary integers do not pass through this table.
#[derive(Debug, Default)]
pub struct LoadMap {
    ranges: Vec<LoadRange>,
    exact: std::collections::HashMap<LinkAddr, u64>,
    strict: bool,
}

impl LoadMap {
    pub fn add_frozen(&mut self, arena: &super::frozen::FrozenArena) {
        self.ranges.push(LoadRange {
            link_start: arena.link_base(),
            runtime_start: arena.runtime_base(),
            len: arena.used(),
        });
    }

    pub fn add_exact(&mut self, link: LinkAddr, runtime: u64) -> Result<(), String> {
        if self.exact.insert(link, runtime).is_some() {
            return Err(format!("duplicate exact load mapping for {:#x}", link.0));
        }
        Ok(())
    }

    pub fn require_mapped(&mut self) {
        self.strict = true;
    }

    pub(crate) fn is_strict(&self) -> bool {
        self.strict
    }

    pub(crate) fn resolves_frozen(&self, addr: LinkAddr) -> bool {
        self.ranges.iter().any(|range| {
            addr.0
                .checked_sub(range.link_start)
                .is_some_and(|off| off < range.len)
        })
    }

    pub fn resolve(&self, addr: LinkAddr) -> Option<u64> {
        if let Some(&runtime) = self.exact.get(&addr) {
            return Some(runtime);
        }
        self.ranges.iter().find_map(|range| {
            let off = addr.0.checked_sub(range.link_start)?;
            (off < range.len).then(|| range.runtime_start + off)
        })
    }

    pub fn resolve_or_identity(&self, addr: LinkAddr) -> Option<u64> {
        self.resolve(addr)
            .or_else(|| (!self.strict).then_some(addr.0))
    }
}

/// One step of an address expression (lower has folded Field/Downcast into Offset).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum PlaceStep {
    /// Read a W64 pointer at the current address and switch the address to it
    Deref,
    /// Constant byte offset; it may be negative, because a slice tail projection `len-k` folds into a
    /// negative term.
    Offset(i32),
    /// DST with a dynamic tail field: `unaligned` is rounded up to the runtime alignment from the
    /// vtable. `packed` is the upper bound on field alignment imposed by an outer `repr(packed(N))`.
    VTableAlignOffset {
        meta: Operand,
        unaligned: u64,
        packed: Option<u64>,
    },
    /// Dynamic index: address += value of frame-local idx slot times stride
    IndexScaled { idx: Slot, stride: u64 },
}

/// Address expression: the engine evaluates it in order, yielding the real address.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlaceExpr {
    pub base: PlaceBase,
    pub steps: Box<[PlaceStep]>,
}

/// Destination of a read or write of a scalar of at most 64 bits.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ScalarPlace {
    /// Fast path: frame-local static slot
    Slot(Slot),
    /// Slow path: scalar at an address expression
    Mem { expr: PlaceExpr, width: Width },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Operand {
    /// Frame-local static slot (fast path)
    Slot(Slot),
    /// Scalar at an address expression
    Mem {
        expr: PlaceExpr,
        width: Width,
    },
    Imm {
        bits: u64,
        width: Width,
    },
    /// Link-time address immediate. This explicit form is the only thing relocated at load time;
    /// an ordinary integer is never guessed to be an address.
    AddrImm(LinkAddr),
    /// The real address of a place itself (indirect argument: passing an aggregate by address)
    AddrOf(PlaceExpr),
    /// Value minus a constant (Subslice slice meta: len' = len - k)
    SubImm {
        base: Box<Operand>,
        sub: u64,
    },
}

impl Operand {
    #[inline]
    pub fn width(&self) -> Width {
        match self {
            Operand::Slot(s) => s.width,
            Operand::Mem { width, .. } => *width,
            Operand::Imm { width, .. } => *width,
            Operand::AddrImm(_) => Width::W64,
            Operand::AddrOf(_) => Width::W64,
            Operand::SubImm { base, .. } => base.width(),
        }
    }
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum IntBinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum IntCc {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum OvfOp {
    Add,
    Sub,
    Mul,
}

/// Scalar floating-point width. f128 is carried by the wide 128-bit channel instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FloatW {
    F16,
    F32,
    F64,
}

/// Scalar-side category of an operand crossing the f128 wide channel (F128FromScalar/F128ToScalar).
/// An integer's width is carried by the statement's `w` field.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum F128Scalar {
    F(FloatW),
    Int { signed: bool },
}

/// f128 unary op: negation or a unary math function.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum F128UnOp {
    Neg,
    Math(MathUnOp),
}

/// Right-hand operand of F128MathBin. `powi` takes the scalar form, everything else the wide form.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum F128Rhs {
    Wide(PlaceExpr),
    Scalar(Operand),
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum FloatOp {
    Add,
    Sub,
    Mul,
    Div,
    /// IEEE fmod (Rust `%` floating-point semantics)
    Rem,
}

/// Math unary ops, used where the float intrinsic has no instruction equivalent and is computed by
/// the host's f32/f64 math instead.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum MathUnOp {
    Sqrt,
    Sin,
    Cos,
    Exp,
    Exp2,
    Ln,
    Log2,
    Log10,
    Fabs,
    Floor,
    Ceil,
    Trunc,
    Round,
    RoundTiesEven,
}

/// Math binary ops (powf/powi/copysign/minnum/maxnum). `powi`'s b operand holds i32 bits.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum MathBinOp {
    Pow,
    Powi,
    Copysign,
    Minnum,
    Maxnum,
}

/// Bitwise unary ops, backing the ctpop/ctlz/cttz/bswap/bitreverse intrinsics.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum BitUnOp {
    Popcount,
    Ctlz,
    Cttz,
    Bswap,
    Bitreverse,
}

/// C++20 memory order, frozen by lower from the atomic intrinsic's const generic `ORD` and mapped to
/// real host atomic instructions. Mapping matters for behaviour, not only speed: collapsing every
/// order to SeqCst is a legal strengthening, but it makes weak-order visibility differ from native,
/// and it makes an x86 Relaxed store pay an xchg for nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MemOrd {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

/// Atomic RMW (fetch_* family); the order comes from `MemOrd`.
/// The signedness of fetch_max/min is frozen by the intrinsic name (atomic_max/min are signed,
/// atomic_umax/umin unsigned), and the executor picks AtomicI* or AtomicU* accordingly.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum RmwOp {
    Xchg,
    Add,
    Sub,
    And,
    Or,
    Xor,
    Nand,
    Max,
    Min,
    UMax,
    UMin,
}

/// SIMD lane element category; every lane operation picks its semantics from it.
/// The category is load-bearing: integer bitops applied to float lanes are silently wrong, because
/// +0.0 == -0.0 is not a bit comparison and NaN is not reflexive. Making the category a field keeps
/// "forgot the category" unrepresentable.
/// Pointer lanes are `Int { signed: false }`: the real-address model passes their bits through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LaneKind {
    Int {
        signed: bool,
    },
    /// f32/f64; the exact width comes from `lane_bytes`. f16/f128 lanes are rejected at lower time.
    Float,
}

/// SIMD per-lane binary op. Signedness and floatness come from `LaneKind`.
/// Comparisons produce mask lanes: an all-1s lane means true.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum SimdBinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Xor,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// Saturating add/sub (integer lanes only)
    SatAdd,
    SatSub,
    /// minimum/maximum_number_nsz, float lanes only. The semantics are minnum/maxnum, which allow
    /// either of +0.0/-0.0 to be returned; host `f::min`/`f::max` is always a legal choice.
    MinNum,
    MaxNum,
    /// Left shift has the same bit-level result for signed/unsigned lanes.
    Shl,
    /// Right shift chooses arithmetic/logical semantics by lane category.
    Shr,
}

/// SIMD per-lane unary op. The float family requires Float lanes and the bit family Int lanes, which
/// lower checks. Per-lane transcendentals call host libm, which is exactly what a native build without
/// fast-math scalarizes to, so the same source yields the same bits.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum SimdUnOp {
    Neg,
    Fabs,
    Fsqrt,
    Ceil,
    Floor,
    Round,
    RoundTiesEven,
    Trunc,
    Fsin,
    Fcos,
    Fexp,
    Fexp2,
    Flog,
    Flog2,
    Flog10,
    Ctlz,
    Cttz,
    Ctpop,
    Bswap,
    Bitreverse,
}

/// SIMD horizontal reduction. Ordered and unordered reductions both fold in lane order: an unordered
/// reduction may use any associative order, and sequential folding is one of them. Float min/max use
/// host `f32::min`/`f64::min`, i.e. minnum/maxnum semantics, which is what LLVM's reduce.fmin/fmax
/// mean.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum SimdReduceOp {
    Add,
    Mul,
    Min,
    Max,
    And,
    Or,
    Xor,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Rvalue {
    Use(Operand),
    /// Real address of this thread's guest TLS instance. `Ctx.tls[id]` is materialized lazily on
    /// first use by heap-allocating and copying the template.
    TlsRef(TlsId),
    // Slice sublength metadata goes through Operand::SubImm and has no rvalue of its own.
    IntBin {
        op: IntBinOp,
        signed: bool,
        a: Operand,
        b: Operand,
    },
    /// Produces a bool (W8).
    IntCmp {
        cc: IntCc,
        signed: bool,
        a: Operand,
        b: Operand,
    },
    /// Bitwise NOT (masked to width)
    NotBits(Operand),
    /// Logical NOT (bool: xor 1) — rustc's Not semantics for bool
    NotBool(Operand),
    /// Two's-complement negation
    Neg(Operand),
    /// IntToInt: truncate then sign-extend from `from` to `to`
    Cast {
        from: (Width, bool),
        to: Width,
        a: Operand,
    },
    /// Take the real address of a place. `Ref` and `RawPtr` share this, since the model uses real
    /// addresses.
    Ref(PlaceExpr),
    /// Pointer arithmetic (`ptr + count * stride`), backing BinOp::Offset and the offset/arith_offset
    /// intrinsics.
    PtrOffset {
        ptr: Operand,
        count: Operand,
        stride: u64,
    },
    /// Three-way compare (BinOp::Cmp), producing a core Ordering as i8: -1, 0 or 1.
    IntCmp3 {
        signed: bool,
        a: Operand,
        b: Operand,
    },
    /// Niche-encoded discriminant read; the Direct encoding dissolves into a Cast at lower time.
    /// `rel = tag - niche_start` wraps at the tag width, and `rel < len` selects
    /// `variants_start + rel`, otherwise the untagged value. Assumes the rustc layout invariant that
    /// a discriminant value equals its variant index.
    NicheDiscr {
        tag: Operand,
        niche_start: u64,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
    },
    /// Floating-point arithmetic on bit patterns: operands and result are f16/f32/f64 bit patterns.
    /// f128 goes through F128Bin instead.
    FloatBin {
        op: FloatOp,
        fw: FloatW,
        a: Operand,
        b: Operand,
    },
    /// Unary math function, computed directly by the host.
    MathUn {
        op: MathUnOp,
        fw: FloatW,
        a: Operand,
    },
    /// Fused multiply-add (fma/fmuladd): computes a*b+c with a single rounding via host `mul_add`.
    /// fmuladd permits a fused or an unfused result, and fusing is always allowed.
    MathFma {
        fw: FloatW,
        a: Operand,
        b: Operand,
        c: Operand,
    },
    MathBin {
        op: MathBinOp,
        fw: FloatW,
        a: Operand,
        b: Operand,
    },
    /// Unsigned max, used to compute an unsized tail alignment as max(sized align, vtable align).
    UMax {
        a: Operand,
        b: Operand,
    },
    /// Float comparison with IEEE semantics: for NaN every predicate is false except Ne. Produces a
    /// bool.
    FloatCmp {
        cc: IntCc,
        fw: FloatW,
        a: Operand,
        b: Operand,
    },
    FloatNeg {
        fw: FloatW,
        a: Operand,
    },
    /// Scalar float conversion among f16/f32/f64; an f128 endpoint uses F128FromScalar/F128ToScalar.
    FloatCast {
        from: FloatW,
        to: FloatW,
        a: Operand,
    },
    /// float to int with Rust `as` semantics: NaN becomes 0 and out-of-range values saturate to the
    /// nearest boundary.
    FloatToInt {
        from: FloatW,
        to: Width,
        signed: bool,
        a: Operand,
    },
    /// int to float.
    IntToFloat {
        from: (Width, bool),
        to: FloatW,
        a: Operand,
    },
    /// f128 comparison of two 16-byte places with IEEE semantics. Produces a bool.
    F128Cmp {
        cc: IntCc,
        a: PlaceExpr,
        b: PlaceExpr,
    },
    /// Bitwise unary op. The operand width is the operation width, so ctlz on W8 counts leading zeros
    /// of an 8-bit value.
    BitUn {
        op: BitUnOp,
        a: Operand,
    },
    /// Atomic load, executed as a real host atomic instruction.
    AtomicLoad {
        addr: Operand,
        width: Width,
        order: MemOrd,
    },
    /// Pointer difference (ptr_offset_from[_unsigned]): (a - b) / stride (i64 division)
    PtrDiff {
        a: Operand,
        b: Operand,
        stride: u64,
    },
    /// SIMD movemask (simd_bitmask): gather each lane's high bit into an integer scalar.
    SimdBitmask {
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// Byte comparison, equivalent to memcmp (the compare_bytes intrinsic). Yields an i32 whose sign
    /// comes from the first differing byte.
    MemCmp {
        a: Operand,
        b: Operand,
        n: Operand,
    },
    /// 128-bit integer comparison of two 16-byte places, as used for TypeId equality. Produces a
    /// bool.
    Cmp128 {
        cc: IntCc,
        signed: bool,
        a: PlaceExpr,
        b: PlaceExpr,
    },
    /// Saturating arithmetic (saturating_add/sub intrinsic)
    IntSat {
        op: OvfOp,
        signed: bool,
        a: Operand,
        b: Operand,
    },
    /// SIMD mask reduction (simd_reduce_all / simd_reduce_any): whether all or any mask lanes are
    /// true. Produces a bool.
    SimdReduce {
        all: bool,
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD arithmetic or bitwise horizontal reduction (simd_reduce_{add,mul}_{ordered,unordered} and
    /// and/or/xor/min/max), folded in lane order. The result is a scalar of the lane width; a float
    /// lane returns a float.
    SimdReduceArith {
        op: SimdReduceOp,
        lane: LaneKind,
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Stmt {
    Assign {
        dst: ScalarPlace,
        rv: Rvalue,
    },
    /// The `*WithOverflow` form: writes the value slot and the overflow flag slot in one go, which is
    /// MIR's `(T, bool)` scalar pair. The two sink offsets come from the frozen pair layout, i.e. the
    /// field offsets of .0 and .1.
    AssignOverflow {
        op: OvfOp,
        signed: bool,
        a: Operand,
        b: Operand,
        dst_val: ScalarPlace,
        dst_flag: ScalarPlace,
    },
    /// Whole-aggregate move with memcpy semantics, used to copy a pair or an aggregate.
    Copy {
        dst: PlaceExpr,
        src: PlaceExpr,
        size: u32,
    },
    /// Repeat fill for the `[expr; N]` rvalue: writes `count` elements of `elem_size` bytes each at
    /// `dst`, taking each element from the scalar `val`. Elements of at most 8 bytes use a scalar
    /// loop; larger ones are memcpy'd.
    RepeatScalar {
        dst: PlaceExpr,
        val: Operand,
        count: u64,
        elem_size: u32,
    },
    /// Atomic store.
    AtomicStore {
        addr: Operand,
        val: Operand,
        order: MemOrd,
    },
    /// Same-width volatile whole read. The executor moves bytes through an alignment=1 opaque
    /// `MaybeUninit` carrier without interpreting aggregate padding. Widths the backend can represent
    /// directly stay as a single volatile event; wider memory-repr values are split into target-sized
    /// chunks.
    VolatileLoad {
        addr: Operand,
        dst: PlaceExpr,
        size: u32,
    },
    /// Same-width volatile whole write; `src` is the bit-pattern source place, padding is moved only as
    /// raw bytes. memory-repr values correspond to rustc's volatile memcpy path. aligned/unaligned
    /// intrinsics have different guest preconditions, but the executor shares an alignment-1 host
    /// carrier to avoid adding extra alignment requirements.
    VolatileStore {
        addr: Operand,
        src: PlaceExpr,
        size: u32,
    },
    /// Atomic compare-exchange: `dst_val` receives the old value and `dst_ok` whether it matched.
    /// Success and failure carry separate orderings.
    AtomicCxchg {
        addr: Operand,
        expected: Operand,
        new: Operand,
        dst_val: ScalarPlace,
        dst_ok: ScalarPlace,
        weak: bool,
        succ: MemOrd,
        fail: MemOrd,
    },
    /// Atomic RMW: dst = old value
    AtomicRmw {
        op: RmwOp,
        addr: Operand,
        val: Operand,
        dst: ScalarPlace,
        order: MemOrd,
    },
    /// Dynamic-length memory copy (copy / copy_nonoverlapping): `count` elements of `elem_size`
    /// bytes each.
    MemCopy {
        dst: Operand,
        src: Operand,
        count: Operand,
        elem_size: u64,
        overlap: bool,
    },
    /// Dynamic-length fill (write_bytes): fills `count` elements of `elem_size` bytes each with the
    /// u8 value `val`.
    MemSet {
        dst: Operand,
        val: Operand,
        count: Operand,
        elem_size: u64,
    },
    /// SIMD per-lane binary op over vector places; the lane geometry is frozen from the layout.
    SimdBin {
        op: SimdBinOp,
        lane: LaneKind,
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD per-lane unary op.
    SimdUn {
        op: SimdUnOp,
        lane: LaneKind,
        dst: PlaceExpr,
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD fused multiply-add (simd_fma / simd_relaxed_fma), float lanes only. It rounds once via
    /// host `mul_add`; the relaxed form allows a fused or unfused result, and fusing is always
    /// allowed.
    SimdFma {
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        c: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD funnel shift (simd_funnel_shl / simd_funnel_shr) on integer lanes, with the shift amount
    /// supplied per lane. A shift at or above the lane bit width is guest UB and terminates loudly.
    SimdFunnel {
        left: bool,
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        shift: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD per-lane cast (simd_cast / simd_as and the pointer family). Both sides have the same lane
    /// count but may differ in width.
    /// A float-to-int cast uses Rust `as` semantics: saturate, and map NaN to 0. For simd_cast an
    /// out-of-range value is guest UB, and saturating is one of the values UB permits.
    SimdCast {
        dst: PlaceExpr,
        src: PlaceExpr,
        lanes: u16,
        src_lane: LaneKind,
        src_bytes: u8,
        dst_lane: LaneKind,
        dst_bytes: u8,
    },
    /// SIMD per-lane select: an all-1s mask lane picks `a` and an all-0s lane picks `b`, which the
    /// mask's type guarantees. The sign bit decides. Mask lanes may be wider than data lanes.
    SimdSelect {
        mask: PlaceExpr,
        mask_bytes: u8,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD bitmask select (simd_select_bitmask): bit i of the scalar mask selects lane i.
    SimdSelectBitmask {
        mask: Operand,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD scattered-address read (simd_gather). A true mask lane reads `*ptr[i]`; a false lane takes
    /// the passthru lane. Access is per-lane, so a false lane must never fake a read, which would
    /// touch memory outside the guest's intent.
    SimdGather {
        passthru: PlaceExpr,
        ptrs: PlaceExpr,
        mask: PlaceExpr,
        mask_bytes: u8,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD scattered-address write (simd_scatter). A false mask lane must never fake a write.
    SimdScatter {
        values: PlaceExpr,
        ptrs: PlaceExpr,
        mask: PlaceExpr,
        mask_bytes: u8,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD contiguous masked load (simd_masked_load): `base` points at a scalar element, so lane i
    /// lives at `base + i * lane_bytes`. False mask lanes take the passthru lane and must never fake
    /// a read.
    SimdMaskedLoad {
        mask: PlaceExpr,
        mask_bytes: u8,
        base: Operand,
        passthru: PlaceExpr,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD contiguous masked store (simd_masked_store). A false mask lane must never fake a
    /// write.
    SimdMaskedStore {
        mask: PlaceExpr,
        mask_bytes: u8,
        base: Operand,
        values: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD dynamic-index extract (simd_extract_dyn). An out-of-bounds index is guest UB and
    /// terminates loudly.
    SimdExtractDyn {
        src: PlaceExpr,
        idx: Operand,
        dst: ScalarPlace,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD dynamic-index insert (simd_insert_dyn): copies `src` whole into `dst`, then overwrites
    /// lane `idx`.
    SimdInsertDyn {
        src: PlaceExpr,
        idx: Operand,
        val: Operand,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD per-lane pointer offset (simd_arith_offset): computes `ptr[i] + offset[i] * stride`,
    /// wrapping, which is what pointer arithmetic means under the real-address model.
    SimdArithOffset {
        ptrs: PlaceExpr,
        offsets: PlaceExpr,
        stride: u64,
        dst: PlaceExpr,
        lanes: u16,
    },
    /// SIMD broadcast (simd_splat / _mm_set1): copies `val` into every lane.
    SimdSplat {
        dst: PlaceExpr,
        val: Operand,
        lanes: u16,
        lane_bytes: u8,
    },
    /// 128-bit integer binary op, computed directly with the host's u128 (read both halves, compute,
    /// write both halves). With `with_overflow`, `dst` holds a `(u128, bool)` layout and the flag is
    /// written at dst+16.
    Bin128 {
        op: IntBinOp,
        signed: bool,
        a: PlaceExpr,
        b: Bin128Rhs,
        dst: PlaceExpr,
        with_overflow: bool,
    },
    /// 128-bit saturating arithmetic, the wide form of saturating_add/saturating_sub, computed
    /// directly with the host's u128/i128.
    Sat128 {
        op: OvfOp,
        signed: bool,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
    },
    /// 128-bit integer to scalar float (u128/i128 as f16/f32/f64), cast directly by the host.
    Wide128ToFloat {
        src: PlaceExpr,
        signed: bool,
        to: FloatW,
        dst: ScalarPlace,
    },
    /// Scalar float to 128-bit integer (f16/f32/f64 as i128/u128), with Rust `as` saturation
    /// semantics.
    FloatToWide128 {
        src: Operand,
        from: FloatW,
        signed: bool,
        dst: PlaceExpr,
    },
    /// 128-bit bitwise unary op whose result is still 128 bits (bswap, bitreverse).
    Bit128 {
        op: BitUnOp,
        src: PlaceExpr,
        dst: PlaceExpr,
    },
    /// 128-bit count-style bitwise unary op (ctpop, ctlz, cttz); the result is a u32 scalar.
    Bit128Count {
        op: BitUnOp,
        src: PlaceExpr,
        dst: ScalarPlace,
    },
    // ===== f128 wide channel: 16-byte values travel through places and are computed directly with
    // the host's f128. rustc lowers the engine's own f128 operations to the same
    // compiler-builtins/__*tf* and glibc *f128 libm symbols the native guest uses, so the same source
    // yields the same bits. =====
    /// f128 arithmetic. `Rem` is fmodf128.
    F128Bin {
        op: FloatOp,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
    },
    /// f128 math binary op. powi's right-hand side is an i32 scalar, the others a wide value.
    F128MathBin {
        op: MathBinOp,
        a: PlaceExpr,
        b: F128Rhs,
        dst: PlaceExpr,
    },
    /// f128 unary op: negation or a unary math function.
    F128Un {
        op: F128UnOp,
        a: PlaceExpr,
        dst: PlaceExpr,
    },
    /// f128 fused multiply-add, rounded once by host `mul_add`.
    F128Fma {
        a: PlaceExpr,
        b: PlaceExpr,
        c: PlaceExpr,
        dst: PlaceExpr,
    },
    /// Scalar (f16/f32/f64 or an integer of at most 64 bits) to f128.
    F128FromScalar {
        src: Operand,
        kind: F128Scalar,
        dst: PlaceExpr,
    },
    /// f128 to scalar: either a cross-cast between float widths or an `as` conversion that saturates
    /// to an integer.
    F128ToScalar {
        src: PlaceExpr,
        kind: F128Scalar,
        w: Width,
        dst: ScalarPlace,
    },
    /// i128/u128 to or from f128, cast directly by the host.
    F128FromWideInt {
        src: PlaceExpr,
        signed: bool,
        dst: PlaceExpr,
    },
    F128ToWideInt {
        src: PlaceExpr,
        signed: bool,
        dst: PlaceExpr,
    },
    /// 128-bit niche discriminant read. `rel = tag - niche_start` wraps at 128 bits, and `rel < len`
    /// selects `variants_start + rel`, otherwise the untagged value.
    NicheDiscr128 {
        tag: PlaceExpr,
        niche_start: u128,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
        dst: ScalarPlace,
    },
    /// Statement-level Trap placeholder: reaching it diagnoses and exits. The block terminator is
    /// still lowered even when this is present, so the Call edges survive and `--vm-stats`
    /// reachability analysis sees them.
    Trap(Box<str>),
    Nop,
    /// Memory fence. A whole-machine fence (atomic_fence) becomes a host `fence`; the single-thread
    /// form (atomic_singlethreadfence) becomes a compiler fence. Both are SeqCst.
    Fence {
        single_thread: bool,
        order: MemOrd,
    },
    /// Aggregate element channel for `[expr; N]`: element 0 is already written, and elements in
    /// `1..count` are filled by copying from it.
    RepeatBytes {
        first: PlaceExpr,
        count: u64,
        elem_size: u64,
    },
}

/// What to do when an unwind passes this call site. The `FrameGuard` carries the equivalent of a
/// dynamic LSDA: `Cleanup` names the block that runs this frame's cleanup chain before the unwind
/// continues.
/// MIR's Unreachable folds into `Continue`, since unwinding out of such a site is UB and the fast
/// path does not check for it.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum UnwindAction {
    Continue,
    Cleanup(Bb),
    /// Unwinding here aborts: a double panic, or an `extern "C"` ABI boundary.
    Terminate,
}

#[cfg(test)]
mod tests {
    use super::program::{
        Block, Builtin, BuiltinCallRole, DecodeQueue, FuncBody, Module, RetAbi, RetDest, Terminator,
    };
    use super::{UnwindAction, Width};

    #[test]
    fn width_roundtrips_supported_byte_sizes_and_masks_values() {
        for (bytes, width, mask) in [
            (1, Width::W8, 0xff),
            (2, Width::W16, 0xffff),
            (4, Width::W32, 0xffff_ffff),
            (8, Width::W64, u64::MAX),
        ] {
            assert_eq!(Width::from_bytes(bytes), Some(width));
            assert_eq!(width.bytes() as u64, bytes);
            assert_eq!(width.mask(), mask);
        }
        assert_eq!(Width::from_bytes(0), None);
        assert_eq!(Width::from_bytes(16), None);
    }

    #[test]
    fn demanded_function_moves_ahead_of_prediction_queue() {
        let mut queue = DecodeQueue {
            predicted: [1, 2, 3].into(),
            queued: [1, 2, 3].into(),
            ..DecodeQueue::default()
        };
        queue.request_demand(3);
        assert_eq!(queue.pop_next(), Some(3));
        assert_eq!(queue.pop_next(), Some(1));
        assert_eq!(queue.pop_next(), Some(2));
    }

    #[test]
    fn capture_rewrite_changes_only_plain_host_syscalls() {
        let body = |name: &str, builtin| FuncBody {
            frame_size: 0,
            frame_align: 1,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin,
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 0,
                    unwind: UnwindAction::Continue,
                    role: BuiltinCallRole::Normal,
                },
            }],
            name: name.into(),
        };
        let mut module = Module {
            funcs: vec![
                body("plain", Builtin::HostSyscall),
                body("already_trace", Builtin::HostSyscallTrace),
                body("other", Builtin::HostWrite),
            ]
            .into(),
            ..Module::default()
        };

        module.rewrite_host_syscalls_for_capture();
        module.rewrite_host_syscalls_for_capture();

        let builtins: Vec<&Builtin> = module
            .funcs
            .iter()
            .map(|body| {
                let Terminator::CallBuiltin { builtin, .. } = &body.blocks[0].term else {
                    unreachable!()
                };
                builtin
            })
            .collect();
        assert!(matches!(builtins[0], Builtin::HostSyscallTrace));
        assert!(matches!(builtins[1], Builtin::HostSyscallTrace));
        assert!(matches!(builtins[2], Builtin::HostWrite));
    }
}
