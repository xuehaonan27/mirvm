//! Spike 1 bytecode: a register machine close to MIR (basic blocks + statements +
//! terminators).
//!
//! Hand-written; spike 1 does no MIR-to-bytecode lowering (lowering is low-risk mechanical
//! work, deferred). Values are u64 words (skeleton; real values need a type/size).
//!
//! Correspondence to MIR: Operand~Operand, Rvalue~Rvalue, Stmt~Statement,
//! Terminator~Terminator, Body~mir::Body, Slot~Local.

pub type Slot = u32;
pub type BlockId = u32;
pub type FuncId = u32;

/// Skeleton version of MIR `Operand::{Copy,Move,Constant}`.
#[derive(Clone, Copy, Debug)]
pub enum Operand {
    Slot(Slot),
    Const(u64),
}

#[derive(Clone, Copy, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Lt,
    Le,
    Eq,
    Gt,
    Ge,
}

/// Skeleton version of MIR `Rvalue`. `Alloc`/`Load` serve the real-address memory goal.
#[derive(Clone, Debug)]
pub enum Rvalue {
    Use(Operand),
    Binary(BinOp, Operand, Operand),
    /// dst = allocate `size` bytes, returning a real address (bump)
    Alloc(Operand),
    /// dst = *(ptr as *const u64)
    Load(Operand),
    /// dst = previous value of an atomic fetch_add(SeqCst) at (ptr, val).
    /// In real bytecode an atomic is an intrinsic **call** in MIR shape, not an Rvalue; this
    /// is spike shorthand. The durable point: an interpreter executing a guest atomic must
    /// issue a **real host atomic instruction** -- plain reads/writes under real threads
    /// would be an engine data race.
    AtomicAdd(Operand, Operand),
}

/// Skeleton version of MIR `Statement`.
#[derive(Clone, Debug)]
pub enum Stmt {
    /// slot = rvalue
    Assign(Slot, Rvalue),
    /// *(ptr as *mut u64) = val
    Store(Operand, Operand),
}

/// Skeleton version of MIR `UnwindAction` (omits Terminate/Unreachable).
#[derive(Clone, Copy, Debug)]
pub enum UnwindAction {
    /// No cleanup at this point in the frame; unwind passes straight through
    Continue,
    /// Run that cleanup block chain first (ending in `Resume`), then keep unwinding
    Cleanup(BlockId),
}

/// Skeleton version of MIR `Terminator`.
#[derive(Clone, Debug)]
pub enum Terminator {
    Goto(BlockId),
    SwitchInt {
        discr: Operand,
        /// (value, target block); jump on match
        targets: Vec<(u64, BlockId)>,
        otherwise: BlockId,
    },
    Call {
        func: FuncId,
        args: Vec<Operand>,
        dst: Slot,
        target: BlockId,
        unwind: UnwindAction,
    },
    /// Drop the value in `slot` (spike semantics: record it in the drop log to verify order;
    /// the real engine calls drop glue)
    Drop {
        slot: Slot,
        target: BlockId,
        unwind: UnwindAction,
    },
    /// Raise a guest panic (approximately a diverging call into the panic runtime; the
    /// unwind edge covers this frame's live Drops)
    Panic {
        payload: Operand,
        unwind: UnwindAction,
    },
    /// Tail of a cleanup block chain: keep propagating outward (approximately MIR
    /// UnwindResume)
    Resume,
    /// Simplified `catch_unwind` intrinsic: normal -> dst/target; guest panic ->
    /// catch_dst/catch_target
    CatchCall {
        func: FuncId,
        args: Vec<Operand>,
        dst: Slot,
        catch_dst: Slot,
        target: BlockId,
        catch_target: BlockId,
    },
    Return,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub term: Terminator,
}

/// One function body. Slot convention: slot 0 = return value, 1..=num_args = params, the
/// rest = locals/temporaries.
#[derive(Clone, Debug)]
pub struct Body {
    pub num_slots: u32,
    pub num_args: u32,
    pub blocks: Vec<Block>,
}

/// A program: a set of functions calling each other by `FuncId` (index).
#[derive(Clone, Debug)]
pub struct Program {
    pub funcs: Vec<Body>,
}
