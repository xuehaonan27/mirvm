//! The vocabulary for naming a value and the memory location it lives in: a width and its mask,
//! the slot fast path, the deref/index projections that produce an address, the scalar places a
//! statement writes, and the operand both are read through.

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
