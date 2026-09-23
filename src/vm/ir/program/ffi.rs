//! The signature a foreign call is made under: the argument and return kinds, the aggregate and
//! leaf classification libffi needs, and the fixed and thunk-argument positions.

/// Category of one libffi argument or return value, frozen at lower time from the function signature's
/// layout.
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
    /// Pass-by-value aggregate, handed off in guest memory at its real address on both sides.
    /// Outbound: the libffi avalue points straight at guest memory, and libffi does the eightbyte
    /// register marshalling itself. Inbound: the closure's avalue points at the bytes and the marshaller
    /// maps them per the callee's ParamAbi, either passing the address for Indirect or reading values in
    /// declared field order for Scalar and Pair.
    Agg(FfiAgg),
}

/// Frozen layout of a pass-by-value aggregate, expanded from the rustc layout: fields in declared
/// order, with padding implied by their offsets.
/// An alignment of at most 8 is the construction boundary, because the result buffer is allocated with
/// 8-byte alignment.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FfiAgg {
    pub size: u32,
    pub align: u32,
    pub fields: Vec<FfiField>,
}

/// One aggregate field: its offset and leaf. Nesting is recursive; ZST members are omitted, and padding
/// is implied by the surrounding size and offsets.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FfiField {
    pub off: u32,
    pub leaf: FfiLeaf,
}

/// An aggregate leaf: either a scalar or a nested aggregate, which is how a ScalarPair `{ptr, len}` or
/// an inner struct of the same shape appears.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FfiLeaf {
    Scalar(FfiKind),
    Agg(FfiAgg),
}

/// Frozen foreign signature. A variadic function freezes its trailing arguments from the call site's
/// actual arguments; `fixed` is how many leading parameters are fixed.
/// The Eq/Hash impls exist because this type keys the thunk cache that maps a function entry address
/// plus signature to the real thunk code address.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ForeignSig {
    pub args: Vec<FfiKind>,
    pub ret: FfiKind,
    /// Some(n) = variadic function, first n are fixed parameters (libffi prep_cif_var)
    pub fixed: Option<usize>,
    /// Positions holding an fn-ptr-typed argument, with the frozen signature of that fn ptr itself.
    /// At runtime the argument at such a position is a fn entry address: a reverse-lookup hit in
    /// `fn_addrs` is swapped for the thunk's real code, while NULL and already-native real code pass
    /// through untouched.
    /// An inner signature's `thunk_args` is always empty, so thunks do not nest.
    pub thunk_args: Vec<(usize, ForeignSig)>,
    /// The ABI's unwind attribute: false for an ordinary C boundary, true for a C-unwind boundary that
    /// lets exceptions through. Direct foreign calls, callbacks and native fn-ptrs all share this
    /// field.
    #[serde(default)]
    pub unwind: bool,
}
