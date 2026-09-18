//! M4 engine bytecode IR (typed; pure Rust, zero rustc types — a fully self-contained frozen artifact).
//!
//! M4.1 upgrade (m4.1-design §3.1): **static slots → place evaluation**. Deref/Index are runtime
//! addresses, so static offsets are insufficient → address expression `PlaceExpr` (lower compiles the
//! projection chain; the engine evaluates it in order to obtain the real address).
//! The frame base is a real address (F6), so frame-local/heap/static accesses all become raw-address
//! reads/writes.
//! Fast path kept: purely frame-local static-offset scalar accesses still use `Slot` (zero evaluation
//! overhead).
//!
//! Separated from the spike bytecode (../bytecode.rs): spikes are frozen validation artifacts; this IR
//! is the real M4 body.

pub type Bb = u32;
pub type FuncId = u32;
/// Inline asm site id (M5.0 asm-stub factory): index into `Module.asm_stub_addrs`.
pub type AsmStubId = u32;

/// A single asm-stub materialization recipe (from M5.0; from A2 symbol names are decoupled from
/// bit order, see `Module.asm_sites`).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AsmSite {
    /// dlsym symbol name of the wrapper (baked into `.globl`/`.type`/`.size` when lower emits GAS text)
    pub name: Box<str>,
    /// Full wrapper GAS text
    pub text: String,
}

/// Scalar width. W128 = two-slot channel (M4.1 step 3).
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

/// InlineAsm input value channel (batch 10 xmm/vector slot 16B channel extension, feeds
/// c_typst_pdf):
/// Scalar = 8B value written low per width; VecBytes = vector byte channel (place real address +
/// full width size, xmm/ymm/zmm = 16/32/64 bytes, same source as wrapper `movups/vmovups` slots).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum AsmIoVal {
    Scalar(Operand),
    VecBytes(PlaceExpr, u32),
}

/// InlineAsm output destination channel (same dual form as `AsmIoVal`).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum AsmIoDst {
    Scalar(ScalarPlace),
    VecBytes(PlaceExpr, u32),
}

/// Frame-local scalar slot (fast path): frozen offset (Field projections already folded into `off`).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Slot {
    pub off: u32,
    pub width: Width,
}

// ===== Place evaluation (M4.1 core) =====

/// Base of an address expression.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum PlaceBase {
    /// Frame-local: real address = frame base + off
    Local(u32),
    /// Frozen-area real address (statics/constant pool, materialized in M4.1 step 4)
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
    /// Read a pointer (W64) at the current address and switch the address to it
    Deref,
    /// Constant byte offset (may be negative — slice tail projection `len-k` folds into a negative term)
    Offset(i32),
    /// DST with a dynamic tail field: `unaligned` must be rounded up to the runtime alignment from the
    /// vtable. `packed` is the upper bound on field alignment imposed by an outer `repr(packed(N))`.
    VTableAlignOffset {
        meta: Operand,
        unaligned: u64,
        packed: Option<u64>,
    },
    /// Dynamic index: address += value of frame-local idx slot × stride
    IndexScaled { idx: Slot, stride: u64 },
}

/// Address expression: the engine evaluates it in order → real address u64.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlaceExpr {
    pub base: PlaceBase,
    pub steps: Box<[PlaceStep]>,
}

/// Scalar place: the destination for reading/writing a ≤64-bit scalar.
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
    /// Link-time address immediate. Only this explicit address form is relocated at load time;
    /// ordinary integers are never guessed to be addresses.
    AddrImm(LinkAddr),
    /// The real address of a place itself (indirect argument = passing an aggregate by address)
    AddrOf(PlaceExpr),
    /// Value minus constant (Subslice slice meta: len' = len − k; M4.4)
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

/// Scalar floating-point width (M5.2 D8c: f16 enters the scalar channel; f128 uses the 128-bit wide
/// channel, not here).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FloatW {
    F16,
    F32,
    F64,
}

/// Scalar-side category for the f128 wide channel (F128From/ToScalar). Int width is in the statement's
/// `w` field.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum F128Scalar {
    F(FloatW),
    Int { signed: bool },
}

/// f128 unary op (Neg + unary math family).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum F128UnOp {
    Neg,
    Math(MathUnOp),
}

/// Right-hand operand of F128MathBin (powi uses an i32 scalar).
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

/// Math unary ops (synthetic handling of must_be_overridden float intrinsics: host f32/f64 direct
/// computation, P7).
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

/// Math binary ops (powf/powi/copysign/minnum/maxnum; powi's b is i32 bits).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum MathBinOp {
    Pow,
    Powi,
    Copysign,
    Minnum,
    Maxnum,
}

/// Bitwise unary ops (builtins for ctpop/ctlz/cttz/bswap/bitreverse intrinsics).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum BitUnOp {
    Popcount,
    Ctlz,
    Cttz,
    Bswap,
    Bitreverse,
}

/// C++20 memory order (M5.2 D8j: lower freezes it from the atomic intrinsic's const generic `ORD`).
/// The old implementation folded everything to SeqCst — correct (a stronger order is an allowed subset)
/// but broke the concurrency-arch "weak memory order natural recovery" promise, and on x86 a Relaxed
/// store paid the xchg cost for nothing. Now the requested order is mapped to host atomic instructions,
/// so weak-order visibility behaves the same as native.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MemOrd {
    Relaxed,
    Acquire,
    Release,
    AcqRel,
    SeqCst,
}

/// Atomic RMW (fetch_* family; order frozen by MemOrd, D8j).
/// The signedness of fetch_max/min is frozen by the intrinsic name (atomic_max/min = signed,
/// atomic_umax/umin = unsigned); the executor selects AtomicI*/AtomicU* accordingly.
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

/// SIMD lane element category (M5.2 D8b): all lane operations dispatch semantics by category.
/// Historical lesson: the M4.1 minimal set treated every lane as integer bitops — float lane add/cmp
/// would be **silently wrong** (+0.0/−0.0 equality, NaN reflexivity are not bit comparisons), and it
/// only didn't blow up because the corpus was all-integer lanes. This type makes "forgot the category"
/// unrepresentable at the type level.
/// Pointer lanes are treated as `Int{signed:false}` (real-address model, bits pass through).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LaneKind {
    Int {
        signed: bool,
    },
    /// f32/f64 (dispatched by lane_bytes; f16/f128 lanes are rejected at lower time, D8c)
    Float,
}

/// SIMD per-lane binary op (M5.2 D8b full family; signedness/float-ness converged into LaneKind).
/// Comparisons produce mask lanes (true = all 1s).
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
    /// minimum/maximum_number_nsz (float lanes only): minnum/maxnum semantics +
    /// the freedom to pick either +0.0/−0.0 — host `f::min/max` (= minnum/maxnum) is always in the
    /// allowed set.
    MinNum,
    MaxNum,
    /// Left shift has the same bit-level result for signed/unsigned lanes.
    Shl,
    /// Right shift chooses arithmetic/logical semantics by lane category.
    Shr,
}

/// SIMD per-lane unary op (M5.2 D8b). Float family requires Float lanes; bit family requires Int lanes
/// (checked at lower time). Transcendental functions per lane call host libm — when native has no
/// fast-math they scalarize to the same libm, so same source means same bits.
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

/// SIMD horizontal reduction (M5.2 D8b): ordered/unordered are both folded in lane order — the
/// "any associative order" set for unordered includes sequential folding, so sequential implementation
/// is always compliant. Float min/max use host `f{32,64}::min/max` (minnum/maxnum semantics, matching
/// LLVM reduce.fmin/fmax).
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
    /// Guest TLS instance real address (M4.4 D3): `Ctx.tls[id]` is materialized lazily (heap alloc +
    /// template copy).
    TlsRef(TlsId),
    // (Subslice meta goes through Operand::SubImm, no independent rvalue)
    IntBin {
        op: IntBinOp,
        signed: bool,
        a: Operand,
        b: Operand,
    },
    /// → bool (W8)
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
    /// Take the real address of a place (Ref/RawPtr share the implementation — real-address model)
    Ref(PlaceExpr),
    /// Pointer arithmetic: ptr + count × stride (BinOp::Offset and offset/arith_offset intrinsics)
    PtrOffset {
        ptr: Operand,
        count: Operand,
        stride: u64,
    },
    /// Three-way compare (BinOp::Cmp) → Ordering (i8: -1/0/1)
    IntCmp3 {
        signed: bool,
        a: Operand,
        b: Operand,
    },
    /// Niche-encoded discriminant read (Direct encoding dissolves into Cast at lower time):
    /// rel = (tag - niche_start) wrapping at tag width; rel < len → variants_start+rel,
    /// otherwise untagged. Niche invariant: discr value == variant index (rustc layout sanity check).
    NicheDiscr {
        tag: Operand,
        niche_start: u64,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
    },
    /// Floating-point arithmetic (bits in, bits out: operands are f16/f32/f64 bit-patterns; f128 uses
    /// F128Bin)
    FloatBin {
        op: FloatOp,
        fw: FloatW,
        a: Operand,
        b: Operand,
    },
    /// Math unary/binary (host direct computation; M4.5 fills in the must_be_overridden float
    /// intrinsic surface)
    MathUn {
        op: MathUnOp,
        fw: FloatW,
        a: Operand,
    },
    /// Fused multiply-add (fma/fmuladd intrinsic, M5.2 D8i): a*b+c single rounding (host mul_add).
    /// fmuladd allows either fused or unfused results; the fused implementation is in the allowed set.
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
    /// Unsigned max (unsized tail alignment = max(sized_align, runtime vtable align), M4.5)
    UMax {
        a: Operand,
        b: Operand,
    },
    /// Float comparison (IEEE semantics, NaN all false except Ne) → bool
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
    /// Scalar float conversion (f16/f32/f64; f128 variants use F128FromScalar/F128ToScalar)
    FloatCast {
        from: FloatW,
        to: FloatW,
        a: Operand,
    },
    /// float → int (Rust `as` saturation semantics: NaN→0, out-of-range→boundary)
    FloatToInt {
        from: FloatW,
        to: Width,
        signed: bool,
        a: Operand,
    },
    /// int → float
    IntToFloat {
        from: (Width, bool),
        to: FloatW,
        a: Operand,
    },
    /// f128 comparison (16-byte place operands) → bool (IEEE semantics)
    F128Cmp {
        cc: IntCc,
        a: PlaceExpr,
        b: PlaceExpr,
    },
    /// Bitwise unary op (semantics by operand width: ctlz(W8) is 8-bit leading zeros)
    BitUn {
        op: BitUnOp,
        a: Operand,
    },
    /// Atomic load (real host atomic instruction — spike4 obligation; SeqCst)
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
    /// SIMD movemask: collect each lane's high bit → integer scalar (simd_bitmask)
    SimdBitmask {
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// Byte comparison (compare_bytes intrinsic = memcmp) → i32 (-1/0/1 semantics by first differing
    /// byte)
    MemCmp {
        a: Operand,
        b: Operand,
        n: Operand,
    },
    /// 128-bit integer comparison (TypeId equality, etc.; operands are 16-byte places) → bool
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
    /// SIMD reduction (simd_reduce_all/any: mask vector all true / any true) → bool
    SimdReduce {
        all: bool,
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD arithmetic/bit horizontal reduction (M5.2 D8b: simd_reduce_{add,mul}_{ordered,unordered}
    /// and and/or/xor/min/max) → lane-width scalar (float returns typed). Folded in lane order.
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
    /// *WithOverflow: writes (value slot, overflow flag slot) in one go — MIR's (T,bool) scalar pair.
    /// The two `dst` offsets come from the frozen pair layout (Field offsets of .0/.1).
    AssignOverflow {
        op: OvfOp,
        signed: bool,
        a: Operand,
        b: Operand,
        dst_val: ScalarPlace,
        dst_flag: ScalarPlace,
    },
    /// Aggregate move (memcpy semantics; channel for pair/aggregate whole copies)
    Copy {
        dst: PlaceExpr,
        src: PlaceExpr,
        size: u32,
    },
    /// Repeat fill: starting at `dst`, `count` elements of `elem_size` bytes each, value from scalar
    /// `src` or memcpy (`[expr; N]` Repeat rvalue; elem ≤8 bytes uses the scalar loop)
    RepeatScalar {
        dst: PlaceExpr,
        val: Operand,
        count: u64,
        elem_size: u32,
    },
    /// Atomic store (SeqCst)
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
    /// Atomic compare-exchange: dst_val = old value, dst_ok = whether it succeeded (succ/fail dual
    /// orders, D8j)
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
    /// Dynamic-length memory copy (copy/copy_nonoverlapping intrinsic: count × elem_size bytes)
    MemCopy {
        dst: Operand,
        src: Operand,
        count: Operand,
        elem_size: u64,
        overlap: bool,
    },
    /// Dynamic-length fill (write_bytes: val is u8, count × elem_size bytes)
    MemSet {
        dst: Operand,
        val: Operand,
        count: Operand,
        elem_size: u64,
    },
    /// SIMD per-lane binary op (dst/a/b are vector places; geometry frozen from layout)
    SimdBin {
        op: SimdBinOp,
        lane: LaneKind,
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD per-lane unary op (M5.2 D8b)
    SimdUn {
        op: SimdUnOp,
        lane: LaneKind,
        dst: PlaceExpr,
        a: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD fused multiply-add (simd_fma/simd_relaxed_fma; Float lanes only, host mul_add single
    /// rounding — relaxed allows fused/unfused, fused is always in the allowed set)
    SimdFma {
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        c: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD funnel shift (simd_funnel_shl/shr; Int lanes, shift is a per-lane vector;
    /// shift ≥ lane bit-width = guest UB → loud termination)
    SimdFunnel {
        left: bool,
        dst: PlaceExpr,
        a: PlaceExpr,
        b: PlaceExpr,
        shift: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD per-lane cast (simd_cast/simd_as/pointer family; lane count is the same on both sides,
    /// widths may differ).
    /// saturate: simd_as float→int semantics (Rust `as`: saturate + NaN→0);
    /// simd_cast out-of-range is guest UB, implementation also saturates (any value is in the allowed
    /// set under UB).
    SimdCast {
        dst: PlaceExpr,
        src: PlaceExpr,
        lanes: u16,
        src_lane: LaneKind,
        src_bytes: u8,
        dst_lane: LaneKind,
        dst_bytes: u8,
    },
    /// SIMD per-lane select (simd_select: mask lane all-1s picks a, all-0s picks b — guaranteed by
    /// type invariant, judged by sign bit; mask vector lane width may differ from data lane width)
    SimdSelect {
        mask: PlaceExpr,
        mask_bytes: u8,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD bitmask select (simd_select_bitmask: scalar mask bit i selects lane i)
    SimdSelectBitmask {
        mask: Operand,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD scattered-address read (simd_gather(val, ptr, mask): mask lane true → read *ptr[i],
    /// false → take passthru lane; per-lane conditional access, false lanes **never fake-read** —
    /// prevents out-of-bounds)
    SimdGather {
        passthru: PlaceExpr,
        ptrs: PlaceExpr,
        mask: PlaceExpr,
        mask_bytes: u8,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD scattered-address write (simd_scatter(val, ptr, mask); false lanes never fake-write)
    SimdScatter {
        values: PlaceExpr,
        ptrs: PlaceExpr,
        mask: PlaceExpr,
        mask_bytes: u8,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD contiguous masked load (simd_masked_load(mask, base, val): base is a pointer to a scalar
    /// element, lane i address = base + i×lane_bytes; false lanes take passthru, never fake-read)
    SimdMaskedLoad {
        mask: PlaceExpr,
        mask_bytes: u8,
        base: Operand,
        passthru: PlaceExpr,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD contiguous masked store (simd_masked_store(mask, base, val); false lanes never fake-write)
    SimdMaskedStore {
        mask: PlaceExpr,
        mask_bytes: u8,
        base: Operand,
        values: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD dynamic-index extract (simd_extract_dyn; out-of-bounds = guest UB → loud termination)
    SimdExtractDyn {
        src: PlaceExpr,
        idx: Operand,
        dst: ScalarPlace,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD dynamic-index insert (simd_insert_dyn: dst = whole copy of src then modify idx lane)
    SimdInsertDyn {
        src: PlaceExpr,
        idx: Operand,
        val: Operand,
        dst: PlaceExpr,
        lanes: u16,
        lane_bytes: u8,
    },
    /// SIMD pointer per-lane offset (simd_arith_offset: ptr[i] + offset[i]×stride,
    /// wrapping — that is the semantics under the real-address model)
    SimdArithOffset {
        ptrs: PlaceExpr,
        offsets: PlaceExpr,
        stride: u64,
        dst: PlaceExpr,
        lanes: u16,
    },
    /// SIMD broadcast (simd_splat / _mm_set1): val copied to every lane
    SimdSplat {
        dst: PlaceExpr,
        val: Operand,
        lanes: u16,
        lane_bytes: u8,
    },
    /// 128-bit integer binary op (host u128 direct: read halves → compute → write halves);
    /// with_overflow: dst is a (u128, bool) layout (flag written at dst+16)
    Bin128 {
        op: IntBinOp,
        signed: bool,
        a: PlaceExpr,
        b: Bin128Rhs,
        dst: PlaceExpr,
        with_overflow: bool,
    },
    /// 128-bit saturating arithmetic (wide form of saturating_add/sub intrinsic; host u128/i128 direct)
    Sat128 {
        op: OvfOp,
        signed: bool,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
    },
    /// 128-bit integer → scalar float (u128/i128 as f16/f32/f64; host direct cast)
    Wide128ToFloat {
        src: PlaceExpr,
        signed: bool,
        to: FloatW,
        dst: ScalarPlace,
    },
    /// Scalar float → 128-bit integer (f16/f32/f64 as i128/u128; `as` saturation semantics, D8k)
    FloatToWide128 {
        src: Operand,
        from: FloatW,
        signed: bool,
        dst: PlaceExpr,
    },
    /// 128-bit bitwise unary op, result still 128 bits (bswap/bitreverse, D8k)
    Bit128 {
        op: BitUnOp,
        src: PlaceExpr,
        dst: PlaceExpr,
    },
    /// 128-bit count-style bitwise unary op (ctpop/ctlz/cttz, result u32 scalar, D8k)
    Bit128Count {
        op: BitUnOp,
        src: PlaceExpr,
        dst: ScalarPlace,
    },
    // ===== f128 wide channel (M5.2 D8c: 16-byte values go through places, host f128 direct —
    // rustc lowers the engine's own f128 operations to the same compiler-builtins/__*tf* +
    // glibc *f128 libm symbols as the native guest, so same source means same bits) =====
    /// f128 arithmetic (includes Rem=fmodf128)
    F128Bin {
        op: FloatOp,
        a: PlaceExpr,
        b: PlaceExpr,
        dst: PlaceExpr,
    },
    /// f128 math binary op (powi's rhs is an i32 scalar, others wide)
    F128MathBin {
        op: MathBinOp,
        a: PlaceExpr,
        b: F128Rhs,
        dst: PlaceExpr,
    },
    /// f128 unary op (negation + all unary math)
    F128Un {
        op: F128UnOp,
        a: PlaceExpr,
        dst: PlaceExpr,
    },
    /// f128 fused multiply-add (host mul_add single rounding)
    F128Fma {
        a: PlaceExpr,
        b: PlaceExpr,
        c: PlaceExpr,
        dst: PlaceExpr,
    },
    /// Scalar (f16/f32/f64/integer ≤64) → f128
    F128FromScalar {
        src: Operand,
        kind: F128Scalar,
        dst: PlaceExpr,
    },
    /// f128 → scalar (float cross-cast / `as` saturate to integer)
    F128ToScalar {
        src: PlaceExpr,
        kind: F128Scalar,
        w: Width,
        dst: ScalarPlace,
    },
    /// i128/u128 ↔ f128 (host cast)
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
    /// 128-bit niche discriminant read (regex_automata's Result<DFA,_> big niche, M4.5):
    /// rel = tag − niche_start (u128 wrapping); rel < len → variants_start+rel, otherwise untagged
    NicheDiscr128 {
        tag: PlaceExpr,
        niche_start: u128,
        variants_start: u64,
        variants_len: u64,
        untagged: u64,
        dst: ScalarPlace,
    },
    /// Statement-level Trap placeholder: reaching it diagnoses and exits, but **the block terminator
    /// is still lowered** — preserving Call edges so `--vm-stats` reachability analysis is accurate
    /// (instrumentation blind-spot fix).
    Trap(Box<str>),
    Nop,
    /// Memory fence (M4.4 D4): atomic_fence → host fence(SeqCst);
    /// single_thread (atomic_singlethreadfence) → compiler_fence(SeqCst)
    Fence {
        single_thread: bool,
        order: MemOrd,
    },
    /// `[expr; N]` aggregate element channel (M4.4): dst[0] already written, fill from it for i∈[1,count)
    RepeatBytes {
        first: PlaceExpr,
        count: u64,
        elem_size: u64,
    },
}

/// Unwind action (full semantics from M4.2: FrameGuard dynamic LSDA, spike3 protocol).
/// MIR Unreachable folds into Continue (unwinding here = UB, fast path does not check).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum UnwindAction {
    Continue,
    Cleanup(Bb),
    /// Unwinding here aborts (double panic / extern "C" ABI boundary)
    Terminate,
}

/// Owned name of an unsupported builtin. The old implementation leaked `&'static str` after
/// deserialization; the Engine now has a real lifetime, so the name is released with the Module.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StaticStr(pub Box<str>);

/// Engine primitives (foreign three-way handling ①, debt-map §2-B): runtime extern boundaries declared
/// by std itself, synthesized into shims by codegen/linker on native — the engine takes over at the
/// same boundary.
/// The alloc-family engine implementation is M4.1 step 5 (heap built-ins); lower inserts a preceding
/// `Stmt::Trap` before they land, to prevent silent failure.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Builtin {
    /// `__rust_alloc(size, align) -> ptr`
    RustAlloc,
    /// `__rust_dealloc(ptr, size, align)`
    RustDealloc,
    /// `__rust_realloc(ptr, old_size, align, new_size) -> ptr`
    RustRealloc,
    /// `__rust_alloc_zeroed(size, align) -> ptr`
    RustAllocZeroed,
    /// `__rust_no_alloc_shim_is_unstable_v2()`: allocation sentinel, no-op
    NoAllocShim,
    /// `_Unwind_RaiseException(exc) -> !`: unwind primitive (M4.2, spike3 raise) —
    /// the host unwinder carries MIRVM's own exception, preserving the guest exception pointer inside;
    /// panic_unwind structures are still managed by the guest standard library in guest heap.
    UnwindRaise,
    /// `catch_unwind(try_fn, data, catch_fn) -> i32` intrinsic (rust_try):
    /// raw unwinder catch + exception category / owning Engine classification + indirect call dispatch.
    /// Only guest panics from the current Engine are handed to `catch_fn`.
    CatchUnwind,
    /// Minimal os:: passthrough (needed by panic chain, real-address zero marshalling; M4.3 replaces
    /// with formal registry dlsym+libffi)
    HostGetenv,
    /// `write(fd, buf, len) -> isize`
    HostWrite,
    /// `strlen(s) -> usize`
    HostStrlen,
    /// `abort() -> !` (libc abort semantics; core::intrinsics::abort also flows here)
    HostAbort,
    /// `fork()` (M5.2 D8f): allowed when guest is single-threaded (child = full process copy,
    /// interpreter state naturally consistent); loudly rejected with multiple guest threads (also a
    /// minefield on native). Unlocks Command::pre_exec and single-threaded daemonize. exec family moved
    /// off the denylist to foreign passthrough (process replacement is inherently correct).
    HostFork,
    /// `atexit(fn)`/`__cxa_atexit(fn,arg,dso)`/`on_exit(fn,arg)`: register guest exit callbacks (D8g).
    /// glibc does not export `atexit` for guest dlsym, so this goes through builtin: the engine keeps a
    /// LIFO registry, and on first registration hooks a native trampoline via the engine's own linked
    /// libc `atexit`; at process teardown guest callbacks are interpreted in LIFO order. Returns 0
    /// (success).
    HostAtexit,
    HostCxaAtexit,
    HostOnExit,
    /// `syscall(nr, ...) -> long` variadic passthrough (dispatched by actual argument count)
    HostSyscall,
    /// `signal(signum, handler)`: guest handler is registered through a stable kernel signal stub into
    /// the owning Engine inbox, then executed at a normal VM safepoint.
    HostSignal,
    /// `raise(signum)`: synchronous signal delivery for the current guest thread. Unlike asynchronous
    /// kernel delivery, the handler must finish before `raise` returns to preserve POSIX nesting order.
    HostRaise,
    /// `sigaction(signum, act, oldact)`: process-level registry keeps the guest-visible handler/mask/
    /// flags, and restores the previous disposition when the Engine shuts down.
    HostSigaction,
    /// A host boundary known to be unsafe to passthrough. Reaching it must fail explicitly, never fake
    /// success. Includes boundaries needing async-signal-safe dedicated implementations, and unwinder
    /// APIs that need guest frame/context translation and cannot expose host interpreter state to the
    /// guest.
    Unsupported(StaticStr),
    /// `_Unwind_DeleteException`: calls the cleanup callback inside the exception object per the Itanium
    /// ABI.
    UnwindDeleteException,
    /// Backtrace shadow frame (M5.2 D8e): Ctx shadow frame stack answers honestly, IP = synthetic fn
    /// token. `_Unwind_Backtrace(trace_fn, arg)` calls guest trace_fn per frame.
    UnwindBacktrace,
    /// `_Unwind_GetIP(ctx)` / `_Unwind_GetIPInfo(ctx, &ip_before)`: read synth ctx IP.
    UnwindGetIp,
    UnwindGetIpInfo,
    /// `_Unwind_GetCFA(ctx)`: reads the object frame stack position in the synthetic context.
    UnwindGetCfa,
    /// `_Unwind_FindEnclosingFunction(ip)`: synthetic IP is the function entry, returns ip itself.
    UnwindFindEnclosing,
    /// Processor hint that does not change guest abstract machine / RAM state (e.g. `pause`,
    /// `vzeroupper`). The interpreter does not persist host vector register state, so it can be
    /// correctly ignored at runtime.
    CpuHintNop,
    /// `core::intrinsics::breakpoint()`: executes real int3 — same SIGTRAP observable behavior as native
    /// (process terminates by default when not traced).
    Breakpoint,
    /// `llvm.x86.addcarry.64(carry, a, b) -> (carry, result)`:
    /// LLVM unadjusted intrinsic pair field order is preserved.
    AddCarry64,
    /// `llvm.x86.subborrow.64(borrow, a, b) -> (borrow, result)`.
    SubBorrow64,
    /// `llvm.x86.xgetbv(xcr) -> u64`: reads the real host extended control register.
    Xgetbv,
    /// x86 vector hardware intrinsics with no portable `simd_*` equivalent. Arguments and return vectors
    /// still pass through the frozen bytecode indirect place ABI; executor helpers call real host
    /// instructions.
    X86Pshufb128,
    X86Pshufb256,
    X86Sha256Msg1,
    X86Sha256Msg2,
    X86Sha256Rnds2,
    /// `llvm.x86.sse2.psad.bw(a, b)` (`_mm_sad_epu8`): sum of absolute differences of two 8-byte
    /// groups, each placed as u64 in qword lane 0/1 (other bits cleared).
    X86PsadBw128,
    /// `llvm.x86.avx2.psad.bw(a, b)` (`_mm256_sad_epu8`): same per 128-bit lane, 4 u64 results total.
    X86PsadBw256,
    /// `llvm.x86.pclmulqdq(a, b, imm8)` (`_mm_clmulepi64_si128`): imm8 bit0/bit4 each select a qword
    /// of a/b for 64×64→128 carryless multiply; other imm8 bits are ignored by hardware.
    X86Pclmulqdq,
    /// `llvm.x86.aesni.aesenc(a, round_key)` etc. AES-NI single-round family (128-bit).
    X86AesEnc,
    X86AesEncLast,
    X86AesDec,
    X86AesDecLast,
    /// `llvm.x86.aesni.aesimc(a)`: InvMixColumns (decryption round-key transformation).
    X86AesImc,
    /// `llvm.x86.aesni.aeskeygenassist(a, imm8)`: SubWord/RotWord ⊕ RCON(=imm8).
    X86AesKeygenAssist,
    /// `llvm.x86.sse42.crc32.32.8/16/32` and `.64.64` (`_mm_crc32_u8/16/32/64`):
    /// CRC32C hardware semantics (reflected polynomial 0x82F63B78 / 64-bit 0xC96C5795D7870F42,
    /// no initial/final inversion — inversion handled by wrapper). Scalar channel.
    X86Crc32U8,
    X86Crc32U16,
    X86Crc32U32,
    X86Crc32U64,
    /// `llvm.x86.avx2.permd(a, idx)` (`_mm256_permutevar8x32_epi32`):
    /// cross-lane dword permute, dst.dword[i] = a.dword[idx.dword[i] & 7].
    X86Permd256,
    /// `llvm.x86.avx2.gather.q.pd.256(src, base, vindex, mask, scale)`:
    /// per-lane conditional gather — mask lane sign bit set reads base+vindex*scale (f64),
    /// otherwise copies src lane; mask-off lanes never touch memory (fault suppression).
    X86GatherQPd256,
    /// `llvm.x86.avx2.gather.d.pd.256`: same, but vindex is 4×i32 (sign-extended to 64 bits for address
    /// arithmetic).
    X86GatherDPd256,
    /// `llvm.x86.avx512.vpmadd52l/h.uq.128/256/512(a, b, c)`: 52-bit unsigned multiply-add,
    /// dst.qword[i] = a[i] + (b[i][51:0]×c[i][51:0]) bit[51:0] (l) or bit[103:52] (h), addition wraps
    /// at 64 bits.
    X86Pmadd52Lo128,
    X86Pmadd52Hi128,
    X86Pmadd52Lo256,
    X86Pmadd52Hi256,
    X86Pmadd52Lo512,
    X86Pmadd52Hi512,
    /// `llvm.x86.ssse3.pmadd.ub.sw.128` / `llvm.x86.avx2.pmadd.ub.sw`
    /// (`_mm(256)_maddubs_epi16`): a unsigned byte × b signed byte, sum of adjacent products
    /// saturates to i16 (simd-adler32 workhorse).
    X86PmaddUbSw128,
    X86PmaddUbSw256,
    /// `llvm.x86.sse2.pmadd.wd` / `llvm.x86.avx2.pmadd.wd` (`_mm(256)_madd_epi16`):
    /// sum of adjacent i16 pair products placed in i32 (MIN×MIN+MIN×MIN wraps to i32::MIN,
    /// hardware-defined).
    X86PmaddWd128,
    X86PmaddWd256,
    /// `llvm.x86.sse3.ldu.dq(p)` (`_mm_lddqu_si128`): unaligned 16-byte pure load
    /// (semantically bit-identical to loadu; corpus batch 8 c_tantivy proved the need).
    X86Lddqu128,
    /// `llvm.x86.avx.ldu.dq.256(p)` (`_mm256_lddqu_si256`): same shape, 32 bytes.
    X86Lddqu256,
    /// `llvm.x86.vcvtps2ph.128(a, rounding)` (`_mm_cvtps_ph`): f32x4 → f16x4 packed into low 64 bits,
    /// high 64 bits cleared. `rounding`: imm[2]=0 → imm[1:0] rounding mode
    /// (0=RNE/1=floor/2=ceil/3=trunc); imm[2]=1 → MXCSR.RC (engine always uses default RNE).
    /// Software model is bit-identical to hardware (NaN: qbit forced + payload shifted right 13 bits;
    /// overflow/subnormal/four rounding modes see x86.rs unit tests).
    X86Cvtps2ph128,
    /// `llvm.x86.vcvtph2ps.128(a)` (`_mm_cvtph_ps`): f16x8 low 64 bits → f32x4,
    /// exact expansion (NaN: qbit forced + payload shifted left 13 bits; subnormals exactly normalized).
    /// Note: recent stdarch `_mm_cvtph_ps` has been portable-ized (simd_shuffle/simd_cast, taking the
    /// f16 lane path rather than this symbol); this symbol is kept for old emit surfaces / direct calls.
    X86Cvtph2ps128,
    /// `llvm.x86.vcvtps2ph.256(a, rounding)` (`_mm256_cvtps_ph`): f32x8 → f16x8, returns 128 bits.
    /// Rounding semantics same as .128.
    X86Cvtps2ph256,
    /// `llvm.x86.vcvtph2ps.256(a)` (`_mm256_cvtph_ps`): f16x8 → f32x8, exact expansion.
    X86Cvtph2ps256,
    /// `llvm.x86.sse.max.ps(a, b)` and `.min` (`_mm_max_ps`/`_mm_min_ps`):
    /// `a>b ? a : b` / `a<b ? a : b` — unordered → second source, ±0 equal → second source,
    /// NaN passes through bit-identically (matches Rust scalar comparison, pinned by tests).
    X86MaxPs128,
    X86MinPs128,
    /// `llvm.x86.avx.max.ps.256` / `.min`: f32x8 per-lane, same semantics as .128.
    X86MaxPs256,
    X86MinPs256,
    /// `llvm.x86.sse.cmp.ps(a, b, imm8)` / `llvm.x86.avx.cmp.ps.256`:
    /// full 32-predicate table (EQ/LT/LE/UNORD/NEQ/NLT/NLE/ORD ×Q/S + EQ_UQ/NGE/NGT/FALSE/
    /// NEQ_OQ/GE/GT/TRUE ×Q/S — S/Q only differ in exception flags, value bits are the same),
    /// true lane becomes all 1s.
    X86CmpPs128,
    X86CmpPs256,
    /// `llvm.x86.sse2.cmp.pd` / `llvm.x86.avx.cmp.pd.256` (same predicate table as cmp.ps,
    /// f64 lane + 64-bit mask; faer default feature V3 kernel proved, C6 on-demand queue)
    X86CmpPd128,
    X86CmpPd256,
    /// `llvm.x86.sse2.max.pd` / `min.pd` / `llvm.x86.avx.max.pd.256` / `min.pd.256`
    /// (maxmin_ps semantics on f64 lanes; faer V3 proved, C6 on-demand queue)
    X86MaxPd128,
    X86MinPd128,
    X86MaxPd256,
    X86MinPd256,
    /// `llvm.x86.sse2.max.sd` / `min.sd` (scalar f64 max/min; faer V3 proved)
    X86MaxSd,
    X86MinSd,
    /// `llvm.x86.sse41.round.ps(a, imm8)` / `llvm.x86.avx.round.ps.256`:
    /// imm[3:0] rounding (0=RNE/1=floor/2=ceil/3=trunc + bit2→MXCSR(=RNE) + bit3 only suppresses
    /// exception flags). NaN: payload preserved + qbit forced (x86.rs explicit arm — libm/roundss NaN
    /// bit behavior drifts with host build target, do not rely on it).
    X86RoundPs128,
    X86RoundPs256,
    /// `llvm.x86.sse2.cvtps2dq(a)` (`_mm_cvtps_epi32`): f32→i32 rounded per MXCSR.RC=RNE;
    /// NaN/out-of-range/±inf → 0x80000000 (indefinite).
    X86CvtPs2dq128,
    /// `llvm.x86.sse2.cvttps2dq(a)` (`_mm_cvttps_epi32`): same but truncate rounding.
    X86CvttPs2dq128,
    /// `llvm.x86.avx.cvt.ps2dq.256` / `.cvtt.ps2dq.256`: f32x8 versions of the above two symbols.
    X86CvtPs2dq256,
    X86CvttPs2dq256,
    /// `llvm.x86.sse41.blendvps(a, b, mask)` / `llvm.x86.avx.blendv.ps.256`:
    /// mask lane sign bit set picks b, cleared picks a (pure bit selection, no arithmetic).
    X86BlendvPs128,
    X86BlendvPs256,
    /// `llvm.x86.sse2.psll.d(a, count)` (`_mm_sll_epi32`): v4i32 logical left shift;
    /// count is a single count value in the low 64 bits of the vector operand, count>31 → all zeros
    /// (tiny-skia lowp u32x4 channel proved). The count vector's high bytes are still read by hardware
    /// as low 64 bits, others ignored.
    X86PsllD128,
    /// `llvm.x86.sse2.psrl.d(a, count)` (`_mm_srl_epi32`): v4i32 logical right shift, same rule.
    X86PsrlD128,
    /// Rewritten from `HostSyscall` at the cold-create boundary of a capture-capable Module.
    /// This internal variant is recorded by the generic builtin helper to avoid adding session checks
    /// to the ordinary syscall/JIT path.
    /// Placed at the end of the enum to keep existing postcard variant numbers unchanged.
    HostSyscallTrace,
}

/// libffi passthrough argument/return category (frozen at lower time from fn sig layout; os:: P7
/// passthrough handling).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FfiKind {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Ptr,
    Void,
    /// Pass-by-value aggregate (C1): bytes are handed off in guest memory at **real address** on both
    /// sides — outbound = libffi avalue points straight at guest memory and handles eightbyte register
    /// marshalling itself; inbound = closure avalue points at the bytes, marshal maps per callee
    /// ParamAbi (Indirect by address / Scalar·Pair reads values in declared field order).
    Agg(FfiAgg),
}

/// C1 pass-by-value aggregate frozen layout (rustc layout expansion; declared-order fields, padding
/// implicit in offsets).
/// align ≤ 8 is the construction boundary (result buffer allocated with 8-byte alignment).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FfiAgg {
    pub size: u32,
    pub align: u32,
    pub fields: Vec<FfiField>,
}

/// C1 aggregate field: offset + leaf (recursive nesting; ZST members omitted, padding kept by
/// size/off).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FfiField {
    pub off: u32,
    pub leaf: FfiLeaf,
}

/// C1 aggregate leaf: scalar or nested aggregate (ScalarPair {ptr,len} / inner struct same shape).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FfiLeaf {
    Scalar(FfiKind),
    Agg(FfiAgg),
}

/// Right-hand operand of Bin128: 128-bit place or ≤64-bit scalar (shift amount for Shl/Shr).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Bin128Rhs {
    Wide(PlaceExpr),
    Scalar(Operand),
}

/// Guest TLS slot id (dense number for `#[thread_local]` statics, M4.4 D3).
pub type TlsId = u32;

/// Guest TLS slot description (frozen at lower time): template = initial bytes' real address in the
/// frozen area (includes relocations), first per-thread access heap-allocates `size` bytes and copies
/// the template. v1 accounting: dtor does not run (design D3).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct TlsSlot {
    pub template: LinkAddr,
    pub size: u64,
    pub align: u32,
}

/// Frozen foreign signature. Variadic functions freeze trailing args per **call-site actuals**
/// (`fixed` = number of fixed parameters).
/// Eq/Hash: thunk factory cache key (M4.4 D1 — (fn entry address, escaped-bit signature) → real code
/// address).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ForeignSig {
    pub args: Vec<FfiKind>,
    pub ret: FfiKind,
    /// Some(n) = variadic function, first n are fixed parameters (libffi prep_cif_var)
    pub fixed: Option<usize>,
    /// fn-ptr type argument bits (M4.4 D1): position + the frozen signature of that fn ptr itself.
    /// At runtime: the actual argument at this position = fn entry address (reverse-lookup hit in
    /// fn_addrs) → swapped to thunk real code; NULL or already-native real code → passed through.
    /// Inner signature's thunk_args is always empty (no nesting).
    pub thunk_args: Vec<(usize, ForeignSig)>,
    /// F-09/R18: preserve ABI unwind attribute. false = ordinary C boundary, true = C-unwind boundary
    /// that allows exceptions through; shared by direct foreign, callback, and native fn-ptr.
    #[serde(default)]
    pub unwind: bool,
}

/// Argument landing position inside the callee frame (engine calling convention v2: actuals are
/// flattened into a `&[u64]` slot sequence).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum ParamAbi {
    /// ZST: occupies no argument slot
    Zst,
    /// Scalar: 1 slot
    Scalar(Slot),
    /// Scalar pair: 2 slots (lo, hi frame-local slots, offsets from frozen pair layout)
    Pair(Slot, Slot),
    /// Large aggregate: 1 slot = source real address; prologue memcpy `size` bytes to frame `off`
    Indirect { off: u32, size: u32 },
}

/// Return channel (engine calling convention v2).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub enum RetAbi {
    Zst,
    /// Scalar: interp_frame returns lo
    Scalar(Slot),
    /// Scalar pair: returns (lo, hi)
    Pair(Slot, Slot),
    /// Large aggregate: caller prepends a hidden first argument = destination real address; on callee
    /// Return, memcpy(hidden pointer slot, _0 slot, size). Hidden pointer slot is appended at frame
    /// tail (sret_off).
    Indirect {
        ret_off: u32,
        size: u32,
        sret_off: u32,
    },
}

/// Return landing point of a Call (caller side).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RetDest {
    /// Ignored (ZST or no destination)
    Ignore,
    Scalar(ScalarPlace),
    /// Pair of half landing points (dst place + frozen half offsets/widths)
    Pair(ScalarPlace, ScalarPlace),
    /// Large aggregate: caller computes the destination real address and passes it as the hidden first
    /// argument (prepended at Call time)
    Indirect(PlaceExpr),
}

/// `SwitchInt` discriminant. Ordinary integers use a scalar operand; i128/u128 stay in a place and are
/// read as full 128 bits at runtime, not truncated to u64 first.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum SwitchDiscr {
    Scalar(Operand),
    Wide(PlaceExpr),
}

/// Role of a direct guest call in the standard startup chain. The vast majority of calls are `Normal`;
/// the fixed toolchain's `lang_start_internal` marks the one `catch_unwind` wrapping user `main` as
/// `MainPanicBoundary`, so the Engine can still keep a structured result after guest std consumes the
/// exception.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CallRole {
    #[default]
    Normal,
    MainPanicBoundary,
}

/// Role of an engine primitive call in the standard startup chain. Only the one fixed-toolchain
/// `std::intrinsics::catch_unwind` call site that passes structural validation is marked
/// `MainPanicCatcher`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BuiltinCallRole {
    #[default]
    Normal,
    MainPanicCatcher,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Terminator {
    Goto(Bb),
    SwitchInt {
        discr: SwitchDiscr,
        targets: Vec<(u128, Bb)>,
        otherwise: Bb,
    },
    Call {
        callee: FuncId,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
        #[serde(default)]
        role: CallRole,
    },
    /// Engine primitive call (not a guest function, no Call edge).
    CallBuiltin {
        builtin: Builtin,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
        #[serde(default)]
        role: BuiltinCallRole,
    },
    /// Foreign passthrough (generic path for os:: P7 handling ①): dlsym + libffi direct call using the
    /// frozen signature — real-address model, zero marshalling (guest pointer is host pointer).
    CallForeign {
        sym: Box<str>,
        sig: ForeignSig,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
    },
    /// Indirect call (fn-ptr / dyn virtual dispatch): callee evaluates to the fn entry real address
    /// (D4), reverse-looked up through `Module.fn_addrs` to FuncId. `--vm-stats` reachability analysis
    /// has no outgoing edge (known blind spot).
    /// null_ok: dyn virtual drop's vtable slot 0 may be null (types without Drop) = no-op.
    /// native_sig (M4.4, second direction of FFI): frozen signature at extern "C" fn-ptr call sites —
    /// reverse-lookup miss = guest holds native real code (runtime dlsym result, e.g.
    /// __pthread_get_minstack) → direct libffi call with this signature; None (Rust ABI / unclassifiable)
    /// means a miss diagnoses and exits.
    CallIndirect {
        callee: Operand,
        args: Vec<Operand>,
        ret: RetDest,
        target: Bb,
        unwind: UnwindAction,
        null_ok: bool,
        native_sig: Option<ForeignSig>,
    },
    /// Inline asm site (M5.0 asm-stub factory, corpus §2.2 three-face destination):
    /// stub indexes `Module.asm_stub_addrs` (load-phase cc assembly + dlopen materialized wrapper real
    /// address, `fn(*mut u8)` slot-buffer ABI). Execution = stack-allocate buf_size buffer, write slots
    /// per `ins`, call real address, read destination per `outs`. All three faces are `unwind
    /// unreachable` (MAY_UNWIND is rejected at lower time).
    InlineAsm {
        stub: AsmStubId,
        buf_size: u32,
        /// (buffer slot offset, input value channel) — scalar writes 8B low in slot; vector byte channel
        /// copies full width
        ins: Vec<(u32, AsmIoVal)>,
        /// (buffer slot offset, output destination channel) — scalar reads 8B low from slot; vector byte
        /// channel copies full width
        outs: Vec<(u32, AsmIoDst)>,
        target: Bb,
    },
    Return,
    Unreachable,
    /// Cleanup chain tail (MIR UnwindResume): only appears in guard.drop cleanup execution — returning
    /// lets the host unwinder continue automatically (spike3: single native stack, zero VM-side
    /// coordination)
    Resume,
    /// MIR UnwindTerminate: abort on reach
    TerminateAbort,
    /// ★ Trap-stub: placeholder for unsupported constructs (core mechanism of the M4 incremental
    /// protocol).
    /// Lowering is total over the collected set — unknown constructs never abort; they are lowered to
    /// Trap locally; only executed paths must be trap-free. The diagnostic string says "which milestone
    /// is still owed".
    Trap(Box<str>),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub term: Terminator,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FuncBody {
    pub frame_size: u32,
    pub frame_align: u32,
    /// Return channel (_0)
    pub ret: RetAbi,
    /// Argument landing positions (_1..=_argc; argument slot order = flattened order)
    pub params: Vec<ParamAbi>,
    /// #[track_caller]: hidden trailing argument &Location's frame-local slot (ABI phantom param,
    /// cg_ssa isomorphic)
    pub caller_loc_off: Option<u32>,
    pub blocks: Vec<Block>,
    /// For diagnostics (symbol name)
    pub name: Box<str>,
}

/// The function table has two ownership modes: lower/L2/image still use an ordinary Vec; `.mirvm`
/// packages keep only the immutable byte snapshot taken at load time, function slice indices, and
/// on-demand publish slots. The interpreter/JIT continue to use the same interface through
/// `len/get/index/iter`.
pub struct FuncTable {
    storage: FuncStorage,
}

enum FuncStorage {
    Eager(Vec<FuncBody>),
    Lazy(LazyFuncs),
}

struct LazyFuncs {
    state: std::sync::Arc<DecodeState>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FuncBlob {
    pub start: usize,
    pub end: usize,
    pub expected_hash: u128,
}

struct DecodeState {
    map: std::sync::Arc<[u8]>,
    blobs: Box<[FuncBlob]>,
    cells: Box<[std::sync::OnceLock<Result<FuncBody, String>>]>,
    queue: std::sync::Mutex<DecodeQueue>,
    ready: std::sync::Condvar,
    done: std::sync::Condvar,
    access: std::sync::Mutex<(Vec<u32>, std::collections::BTreeSet<u32>)>,
    heat_path: std::path::PathBuf,
}

#[derive(Default)]
struct DecodeQueue {
    demand: std::collections::VecDeque<usize>,
    predicted: std::collections::VecDeque<usize>,
    queued: std::collections::BTreeSet<usize>,
    stop: bool,
}

impl DecodeQueue {
    fn request_demand(&mut self, index: usize) {
        if self.queued.insert(index) {
            self.demand.push_back(index);
        } else if let Some(at) = self.predicted.iter().position(|item| *item == index) {
            self.predicted.remove(at);
            self.demand.push_back(index);
        }
    }

    fn pop_next(&mut self) -> Option<usize> {
        let index = self
            .demand
            .pop_front()
            .or_else(|| self.predicted.pop_front())?;
        self.queued.remove(&index);
        Some(index)
    }
}

impl Default for FuncTable {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl From<Vec<FuncBody>> for FuncTable {
    fn from(funcs: Vec<FuncBody>) -> Self {
        Self {
            storage: FuncStorage::Eager(funcs),
        }
    }
}

impl FromIterator<FuncBody> for FuncTable {
    fn from_iter<T: IntoIterator<Item = FuncBody>>(iter: T) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl FuncTable {
    pub(crate) fn from_bytes(
        map: std::sync::Arc<[u8]>,
        blobs: Vec<FuncBlob>,
        heat_path: std::path::PathBuf,
    ) -> Self {
        let predicted = read_heat_order(&heat_path, blobs.len());
        let state = std::sync::Arc::new(DecodeState {
            map,
            cells: (0..blobs.len())
                .map(|_| std::sync::OnceLock::new())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            blobs: blobs.into_boxed_slice(),
            queue: std::sync::Mutex::new(DecodeQueue {
                predicted: predicted.iter().copied().collect(),
                queued: predicted.into_iter().collect(),
                ..DecodeQueue::default()
            }),
            ready: std::sync::Condvar::new(),
            done: std::sync::Condvar::new(),
            access: std::sync::Mutex::new((Vec::new(), std::collections::BTreeSet::new())),
            heat_path,
        });
        let worker_state = std::sync::Arc::clone(&state);
        let worker = std::thread::Builder::new()
            .name("mirvm-decode".into())
            .spawn(move || decode_worker(worker_state))
            .ok();
        if worker.is_some() {
            state.ready.notify_one();
        }
        Self {
            storage: FuncStorage::Lazy(LazyFuncs { state, worker }),
        }
    }

    pub fn len(&self) -> usize {
        match &self.storage {
            FuncStorage::Eager(funcs) => funcs.len(),
            FuncStorage::Lazy(lazy) => lazy.state.blobs.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&FuncBody> {
        if index >= self.len() {
            return None;
        }
        Some(match &self.storage {
            FuncStorage::Eager(funcs) => &funcs[index],
            FuncStorage::Lazy(lazy) => lazy.get(index),
        })
    }

    pub fn iter(&self) -> FuncIter<'_> {
        FuncIter {
            funcs: self,
            next: 0,
        }
    }

    fn iter_mut(&mut self) -> std::slice::IterMut<'_, FuncBody> {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.storage else {
            unreachable!()
        };
        funcs.iter_mut()
    }

    pub fn push(&mut self, body: FuncBody) {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.storage else {
            unreachable!()
        };
        funcs.push(body);
    }

    pub fn drain_into(&mut self, out: &mut Vec<FuncBody>) {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.storage else {
            unreachable!()
        };
        out.append(funcs);
    }

    pub(crate) fn flush_heat_order(&self) {
        if let FuncStorage::Lazy(lazy) = &self.storage {
            write_heat_order(&lazy.state);
        }
    }

    fn make_eager(&mut self) {
        if matches!(self.storage, FuncStorage::Eager(_)) {
            return;
        }
        let funcs = self.iter().cloned().collect();
        self.storage = FuncStorage::Eager(funcs);
    }
}

impl LazyFuncs {
    fn get(&self, index: usize) -> &FuncBody {
        record_access(&self.state, index as u32);
        if self.worker.is_none() {
            decode_one(&self.state, index);
        } else if self.state.cells[index].get().is_none() {
            let mut queue = self.state.queue.lock().unwrap();
            if self.state.cells[index].get().is_none() {
                queue.request_demand(index);
                self.state.ready.notify_one();
                while self.state.cells[index].get().is_none() {
                    queue = self.state.done.wait(queue).unwrap();
                }
            }
        }
        match self.state.cells[index].get().expect("function decode slot not published") {
            Ok(body) => body,
            Err(error) => panic!("verified function failed during lazy decode: {error}"),
        }
    }
}

impl Drop for LazyFuncs {
    fn drop(&mut self) {
        write_heat_order(&self.state);
        {
            let mut queue = self.state.queue.lock().unwrap();
            queue.stop = true;
            self.state.ready.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn decode_worker(state: std::sync::Arc<DecodeState>) {
    loop {
        let index = {
            let mut queue = state.queue.lock().unwrap();
            loop {
                if queue.stop {
                    return;
                }
                if let Some(index) = queue.pop_next() {
                    break index;
                }
                queue = state.ready.wait(queue).unwrap();
            }
        };
        decode_one(&state, index);
        state.done.notify_all();
    }
}

fn decode_one(state: &DecodeState, index: usize) {
    if state.cells[index].get().is_some() {
        return;
    }
    let blob = state.blobs[index];
    let bytes = &state.map[blob.start..blob.end];
    let decoded = if func_blob_hash(bytes) != blob.expected_hash {
        Err(format!(
            "function {index} changed after package verification"
        ))
    } else {
        postcard::from_bytes(bytes)
            .map_err(|error| format!("function {index} decode failed: {error}"))
    };
    let _ = state.cells[index].set(decoded);
}

fn func_blob_hash(data: &[u8]) -> u128 {
    let fnv = |prefix: &[u8]| {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in prefix.iter().chain(data) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    };
    ((fnv(&[]) as u128) << 64) | u128::from(fnv(b"\x01mirvmar"))
}

fn record_access(state: &DecodeState, id: u32) {
    let mut access = state.access.lock().unwrap();
    if access.1.insert(id) {
        access.0.push(id);
    }
}

fn read_heat_order(path: &std::path::Path, count: usize) -> Vec<usize> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    text.split_ascii_whitespace()
        .filter_map(|value| value.parse::<usize>().ok())
        .filter(|id| *id < count && seen.insert(*id))
        .collect()
}

fn write_heat_order(state: &DecodeState) {
    let access = state.access.lock().unwrap();
    if access.0.is_empty() {
        return;
    }
    let Some(dir) = state.heat_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let body = access
        .0
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    let tmp = dir.join(format!(".heat-{}.tmp", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(tmp, &state.heat_path);
    }
}

pub struct FuncIter<'a> {
    funcs: &'a FuncTable,
    next: usize,
}

impl<'a> Iterator for FuncIter<'a> {
    type Item = &'a FuncBody;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.funcs.get(self.next)?;
        self.next += 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.funcs.len().saturating_sub(self.next);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for FuncIter<'_> {}

impl<'a> IntoIterator for &'a FuncTable {
    type Item = &'a FuncBody;
    type IntoIter = FuncIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl std::ops::Index<usize> for FuncTable {
    type Output = FuncBody;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("FuncId outside function table")
    }
}

impl std::fmt::Debug for FuncTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl serde::Serialize for FuncTable {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> serde::Deserialize<'de> for FuncTable {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <Vec<FuncBody> as serde::Deserialize>::deserialize(deserializer).map(Self::from)
    }
}

/// main startup plan (isomorphic to cg_ssa create_entry_fn):
/// `lang_start(main fn-ptr, argc, argv, sigpipe) -> isize` (return value = process exit code).
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct EntryPlan {
    pub lang_start: FuncId,
    /// User main's D4 entry real address (first argument to lang_start, dispatched via CallIndirect)
    pub main_addr: LinkAddr,
    pub argc: u64,
    /// Real address of the argv C-string pointer table (frozen area)
    pub argv_ptr: u64,
    pub sigpipe: u8,
}

/// P2 GOT symbol table entry (decision-history §7.5c): weak = on miss write 0, do not abort
/// (extern weak symbol absent address = NULL semantics).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GotSym {
    pub name: Box<str>,
    pub weak: bool,
}

/// P2 startup-phase fixup point (decision-history §7.5c): load phase writes
/// `*addr = resolve(foreign_syms[sym]) + addend`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct GotFixup {
    pub addr: LinkAddr,
    pub sym: u32,
    pub addend: u64,
}

/// One object pointer inside the frozen bytes. `at` is the 8-byte cell to write, `target` is the link
/// address it should point to (addend already folded); at instantiation both ends are translated via
/// LoadMap before writing.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct FrozenReloc {
    pub at: LinkAddr,
    pub target: FrozenRelocTarget,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum FrozenRelocTarget {
    Frozen(LinkAddr),
    Entry(LinkAddr),
}

/// P1 entry executable recipe (decision-history §7.6). The artifact only stores the guest fn's logical
/// address, FuncId, and frozen C ABI signature; each Engine materializes a unique closure at startup
/// via libffi. Real fn-ptrs are not cached or packaged.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EntryStubSite {
    /// Logical identity shared by all fn-ptr references to this fn in the artifact; each Engine maps it
    /// to a unique closure.
    pub link_addr: LinkAddr,
    pub func: FuncId,
    pub sig: ForeignSig,
}

/// Hidden ELF symbol used by native bridges that call back into a guest entry.
/// The symbol identifies the artifact address only; each Engine writes its own
/// runtime closure address into the corresponding slot while instantiating.
pub(crate) fn native_entry_slot_name(link_addr: LinkAddr) -> String {
    format!("__mirvm_p1_target_{:016x}", link_addr.0)
}

/// FuncIds for the four `__rust_*` shims of a custom `#[global_allocator]` (corpus batch 7
/// c_mimalloc proved the fix): rustc_ast::expand::global_allocator generates four local forwarding
/// fns (body = call each method of the user's GlobalAlloc) for crates with that attribute.
/// Allocation semantics are **program-level**: the base/deps image's `CallBuiltin(Rust*)` arms baked
/// with the Default session and the delta/image's guest shim must route to the **same**
/// allocator — otherwise cross-heap free causes mimalloc metadata SIGSEGV. At runtime interp uses this
/// field to unify upward routing, regardless of which session the bytecode was baked in.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct AllocShims {
    pub alloc: FuncId,
    pub dealloc: FuncId,
    pub realloc: FuncId,
    pub alloc_zeroed: FuncId,
}

/// Guest-side resource reclamation plan for uncaught guest panics.
///
/// `cleanup` is the object function `std::panicking::catch_unwind::cleanup` in the fixed toolchain:
/// it receives the panic_unwind raw exception pointer, takes out the `Box<dyn Any + Send>`, and
/// decrements the guest panic count. `drop_payload` is the drop glue for that Box, responsible for
/// running the user payload's Drop and freeing memory through the guest's own global allocator. The
/// engine only moves two opaque machine words; it does not read std's private Exception/Box/vtable
/// layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GuestPanicCleanup {
    pub cleanup: FuncId,
    pub drop_payload: FuncId,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Module {
    pub funcs: FuncTable,
    /// Lightweight FuncId-order object symbol name index. When the package keeps function bodies lazy,
    /// backtrace can still build a standard ELF symbol table without decoding every FuncBody.
    pub function_names: Vec<Box<str>>,
    /// FuncId → address in the current process symbol ELF; generated at load time, not cached or
    /// packaged.
    #[serde(skip)]
    pub backtrace_ips: Vec<u64>,
    /// Keeps the in-memory ELF file and dlopen object alive for the Engine lifetime.
    #[serde(skip)]
    pub backtrace_image: Option<super::backtrace::SymbolImage>,
    /// Exported name (no_mangle symbol) → FuncId, used by --vm-call lookup
    pub exports: std::collections::HashMap<Box<str>, FuncId>,
    /// Frozen area (statics/constant pool/fn entries; materialized by lower, read-only after publish —
    /// except static mut)
    pub frozen: Option<super::frozen::FrozenArena>,
    /// This Module instance's link address → runtime address mapping. Built at package/image
    /// instantiation, not part of the artifact.
    #[serde(skip)]
    pub load_map: LoadMap,
    /// fn-ptr entry real address → FuncId (D4 reverse lookup; indirect call dispatch M4.1 step 5)
    pub fn_addrs: std::collections::HashMap<u64, FuncId>,
    /// Artifact-address form of `fn_addrs`. Rebuilt after dynamic instance or P1 closure addresses
    /// change.
    pub link_fn_addrs: std::collections::HashMap<LinkAddr, FuncId>,
    /// P1 closure addresses already materialized by this Engine and directly usable by native.
    #[serde(skip)]
    pub executable_entry_addrs: std::collections::HashSet<u64>,
    /// Optional shared library candidate paths from `-l` link directives; if one does not exist, other
    /// candidates are tried.
    pub native_libs: Vec<Box<str>>,
    /// Shared libraries already materialized by the loading phase that must dlopen successfully before
    /// executing foreign code (currently M5.1 Static archive `.a → .so` products). Failure must not
    /// degrade to an ordinary dlsym miss.
    pub required_native_libs: Vec<Box<str>>,
    /// Expected content identity for each required native library. A zero
    /// value denotes a raw in-process Module whose caller did not supply an
    /// artifact hash; Package instances always carry and verify this list.
    #[serde(skip)]
    pub required_native_hashes: Vec<u128>,
    /// Per-Engine self-produced shared library images that have finished system-dependency resolution
    /// and ELF relocation but are explicitly managed by the Engine for init/fini; not cached or
    /// packaged.
    #[serde(skip)]
    pub native_images: Vec<super::native_instance::NativeImage>,
    /// This package's self-loaded machine-code image. Visible only to this Module's foreign resolution,
    /// avoiding global_asm name collisions across Engines; not serialized, rebuilt from MC section on
    /// package load.
    #[serde(skip)]
    pub mc_images: Vec<super::mcload::McImage>,
    /// Guest TLS slot table (M4.4 D3: TlsId → template/size; per-thread instance in Ctx.tls)
    pub tls: Vec<TlsSlot>,
    /// asm-stub wrapper real addresses (M5.0): AsmStubId → `fn(*mut u8)` machine address (load phase
    /// cc assembly + dlopen + dlsym materialization). Execution phase only reads u64 and calls directly,
    /// purity preserved.
    /// **Not part of L2 snapshot semantics** — warm path materializes idempotently from asm_sites and
    /// overwrites.
    pub asm_stub_addrs: Vec<u64>,
    /// asm-stub materialization recipe (M6 slice 2): symbol name + wrapper GAS full text, ordered by
    /// AsmStubId (= bit order). Warm load reruns asm::materialize with it (if content hash hits the .so
    /// cache, only dlopen+dlsym; if cleared, re-cc self-heals). **Symbol names decoupled from bit
    /// order**: A2 split mode only knows final bit order at the end, so class-prefix names
    /// (mirvm_asm_xi{j}/xd{k}) are used; non-split paths keep the positional names mirvm_asm_{id}.
    pub asm_sites: Vec<AsmSite>,
    /// P2 GOT symbol table (decision-history §7.5c): slots are ordinary 8-byte cells in the frozen area
    /// (this field); bytecode/frozen bytes bake slot addresses, not values. At startup names are
    /// re-resolved and each fixup point rewrites the content, keeping the module position-independent
    /// across ASLR. Image sides each have their own table and are merged by name on absorb.
    pub foreign_syms: Vec<GotSym>,
    /// P2 startup-phase fixup points: `*(addr) = resolve(foreign_syms[sym]) + addend`; addr is this
    /// module's frozen-domain LinkAddr, and the real slot is found via LoadMap after instantiation.
    pub got_fixups: Vec<GotFixup>,
    /// Object pointer relocations inside/between frozen domains, excluding foreign GOT fixup points.
    pub frozen_relocs: Vec<FrozenReloc>,
    /// P1 entry executable recipe (decision-history §7.6): guest fns in this domain that are address-
    /// taken and exportable with C ABI; each Engine builds unique closures and LinkAddr mappings from
    /// these recipes.
    pub entry_stub_sites: Vec<EntryStubSite>,
    /// Old code-area handle used by lower for allocating stable logical addresses. Mapping is released
    /// when starting the Engine; runtime only executes per-Engine libffi closures; this field is not
    /// part of the artifact.
    #[serde(skip)]
    pub entry_stubs: super::codearena::StubArena,
    /// absorb-mounted image/base entry logical-address domains and recipes:
    /// (link-address domain base, recipe, lower-time address allocation handle).
    #[serde(skip)]
    pub image_entry_stubs: Vec<(usize, Vec<EntryStubSite>, super::codearena::StubArena)>,
    /// `#[global_allocator]` custom `__rust_*` shims (see AllocShims note): kind=Global is registered
    /// on the delta side, and at runtime interp CallBuiltin(Rust*) arms unify routing.
    pub custom_alloc_shims: Option<AllocShims>,
    /// Object-side cleanup plan the Engine top level must execute after catching an uncaught guest
    /// panic. Hand-built test Modules and non-executable image stack layers may be None; all executable
    /// full/delta lower products must have one, and the real run entry must refuse None rather than
    /// leak.
    pub guest_panic_cleanup: Option<GuestPanicCleanup>,
    /// main startup chain (M4.3; None in --vm-call mode)
    pub entry: Option<EntryPlan>,
    /// S4/S3′ image-stack frozen areas (absorb mounts base + each dependency image's frozen area,
    /// same lifetime as this module — delta bytecode embeds cross-domain absolute addresses, and those
    /// domains must live until guest exit).
    /// **Not part of L2 snapshot** — image files have their own lifecycles, delta entries only refer by
    /// key chain (ircache double verification).
    #[serde(skip)]
    pub image_frozens: Vec<super::frozen::FrozenArena>,
}

impl Module {
    /// Translate a link-time address to this Module instance's runtime address.
    /// Cold lower products keep the identity mapping until dynamic load mapping is attached.
    pub fn resolve_link_addr(&self, addr: LinkAddr) -> u64 {
        self.load_map
            .resolve_or_identity(addr)
            .unwrap_or_else(|| panic!("unmapped artifact address {:#x}", addr.0))
    }

    pub fn try_resolve_link_addr(&self, addr: LinkAddr) -> Result<u64, String> {
        self.load_map
            .resolve_or_identity(addr)
            .ok_or_else(|| format!("unmapped artifact address {:#x}", addr.0))
    }

    pub fn is_executable_entry(&self, addr: u64) -> bool {
        self.executable_entry_addrs.contains(&addr)
    }

    pub fn rebuild_load_map(&mut self) {
        let mut map = LoadMap::default();
        if let Some(frozen) = &self.frozen {
            map.add_frozen(frozen);
        }
        for frozen in &self.image_frozens {
            map.add_frozen(frozen);
        }
        self.load_map = map;
    }

    pub fn apply_frozen_relocs(&self) -> Result<(), String> {
        for (index, reloc) in self.frozen_relocs.iter().enumerate() {
            let at = self
                .load_map
                .resolve(reloc.at)
                .ok_or_else(|| format!("frozen relocation {index} write address is unmapped"))?;
            let target_link = match reloc.target {
                FrozenRelocTarget::Frozen(addr) | FrozenRelocTarget::Entry(addr) => addr,
            };
            let target = self.load_map.resolve(target_link).ok_or_else(|| {
                format!(
                    "frozen relocation {index} target address {:#x} ({:?}) is unmapped",
                    target_link.0, reloc.target
                )
            })?;
            unsafe { (at as *mut u64).write_unaligned(target) };
        }
        Ok(())
    }

    pub fn rebuild_fn_addrs(&mut self) {
        if self.link_fn_addrs.is_empty() {
            self.link_fn_addrs = self
                .fn_addrs
                .iter()
                .map(|(&addr, &func)| (LinkAddr(addr), func))
                .collect();
        }
        self.fn_addrs = self
            .link_fn_addrs
            .iter()
            .map(|(&addr, &func)| (self.resolve_link_addr(addr), func))
            .collect();
    }

    pub fn ensure_function_names(&mut self) {
        if self.function_names.len() != self.funcs.len() {
            self.function_names = self.funcs.iter().map(|body| body.name.clone()).collect();
        }
    }

    /// Freeze this Engine instance into the capture-capable execution domain.
    /// The serialized Module stays plain; rewriting happens only after a session
    /// has armed capture and before `Shared` publishes the Module for execution.
    pub(crate) fn rewrite_host_syscalls_for_capture(&mut self) {
        for body in self.funcs.iter_mut() {
            for block in &mut body.blocks {
                let Terminator::CallBuiltin { builtin, .. } = &mut block.term else {
                    continue;
                };
                if matches!(builtin, Builtin::HostSyscall) {
                    *builtin = Builtin::HostSyscallTrace;
                }
            }
        }
    }
    /// argv C-string table finalization (isomorphic to tier-0 setup_process_memory; moved from lower
    /// starting M6 slice 2).
    /// argv is a **runtime input**: it must not enter the L2 cache snapshot; cold/hot paths both
    /// append allocation and backfill after the snapshot each run — single code path, eliminating
    /// cold/hot drift.
    pub fn finalize_entry_argv(&mut self, argv: &[String]) -> Result<(), String> {
        let Some(entry) = self.entry.as_mut() else {
            return Ok(());
        };
        let frozen = self
            .frozen
            .as_mut()
            .ok_or("executable module has no frozen memory for argv")?;
        let mut ptrs: Vec<u64> = Vec::with_capacity(argv.len());
        for a in argv {
            let bytes = a.as_bytes();
            let p = frozen.alloc(bytes.len() as u64 + 1, 1);
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
                *((p + bytes.len() as u64) as *mut u8) = 0;
            }
            ptrs.push(p);
        }
        let table = frozen.alloc((ptrs.len() as u64 + 1) * 8, 8);
        for (i, &p) in ptrs.iter().enumerate() {
            unsafe { *((table + i as u64 * 8) as *mut u64) = p };
        }
        // trailing NULL is guaranteed by zeroing
        entry.argc = argv.len() as u64;
        entry.argv_ptr = table;
        Ok(())
    }

    /// GOT merge (P2, S4/S3′ absorb): image-side symbol table is merged into this module — symbols
    /// deduplicated by name, fixup sym indices remapped to merged idx; fixup addr is in the image spline
    /// domain (fixed base), and after merge still points to the same frozen cell, taken over as-is.
    pub fn absorb_got(&mut self, syms: Vec<GotSym>, mut fixups: Vec<GotFixup>) {
        if fixups.is_empty() {
            return;
        }
        let mut remap: Vec<u32> = Vec::with_capacity(syms.len());
        for s in syms {
            let idx = match self.foreign_syms.iter().position(|e| e.name == s.name) {
                Some(i) => {
                    // F-08: merge also does weak/strong merge (any strong makes strong)
                    if !s.weak {
                        self.foreign_syms[i].weak = false;
                    }
                    i as u32
                }
                None => {
                    self.foreign_syms.push(s);
                    (self.foreign_syms.len() - 1) as u32
                }
            };
            remap.push(idx);
        }
        for f in &mut fixups {
            f.sym = remap[f.sym as usize];
        }
        self.got_fixups.append(&mut fixups);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Block, Builtin, BuiltinCallRole, DecodeQueue, FuncBody, Module, RetAbi, RetDest,
        Terminator, UnwindAction, Width,
    };

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
