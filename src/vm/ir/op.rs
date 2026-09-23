//! The operations the instruction families are written in: integer arithmetic and comparison,
//! the checked family, floats and f128, the math and bit operations, the read-modify-write pair,
//! and the SIMD lane and reduction operations.

use super::*;

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
