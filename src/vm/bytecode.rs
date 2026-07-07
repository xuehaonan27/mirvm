//! Spike 1 字节码：寄存器式、贴近 MIR（基本块 + 语句 + 终止子）。
//!
//! 手写（Spike 1 不做 MIR→字节码降低——降低是低风险机械活，后置）。
//! 值为 u64 word（skeleton；真值需带类型/尺寸，见 spike1 文档教训）。
//!
//! 与 MIR 的对应：Operand≈Operand、Rvalue≈Rvalue、Stmt≈Statement、
//! Terminator≈Terminator、Body≈mir::Body、Slot≈Local。

pub type Slot = u32;
pub type BlockId = u32;
pub type FuncId = u32;

/// MIR `Operand::{Copy,Move,Constant}` 的骨架版。
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

/// MIR `Rvalue` 的骨架版。`Alloc`/`Load` 为次要目标 1b（真地址内存）。
#[derive(Clone, Debug)]
pub enum Rvalue {
    Use(Operand),
    Binary(BinOp, Operand, Operand),
    /// dst = 分配 `size` 字节，返回真地址（bump）
    Alloc(Operand),
    /// dst = *(ptr as *const u64)
    Load(Operand),
}

/// MIR `Statement` 的骨架版。
#[derive(Clone, Debug)]
pub enum Stmt {
    /// slot = rvalue
    Assign(Slot, Rvalue),
    /// *(ptr as *mut u64) = val
    Store(Operand, Operand),
}

/// MIR `UnwindAction` 的骨架版（略 Terminate/Unreachable）。
#[derive(Clone, Copy, Debug)]
pub enum UnwindAction {
    /// 本帧此处无清理，unwind 直接穿过
    Continue,
    /// 先跑该 cleanup 块链（以 `Resume` 结束），再继续 unwind
    Cleanup(BlockId),
}

/// MIR `Terminator` 的骨架版。
#[derive(Clone, Debug)]
pub enum Terminator {
    Goto(BlockId),
    SwitchInt {
        discr: Operand,
        /// (值, 目标块)；命中即跳
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
    /// 释放槽中的值（spike 语义：记入 drop 日志以验证顺序；真身 = drop glue 调用）
    Drop {
        slot: Slot,
        target: BlockId,
        unwind: UnwindAction,
    },
    /// 发起 guest panic（≈ 调 panic 运行时的 diverging call；unwind 边覆盖本帧 live Drop）
    Panic {
        payload: Operand,
        unwind: UnwindAction,
    },
    /// cleanup 块链尾：继续向外传播（≈ MIR UnwindResume）
    Resume,
    /// ≈ `catch_unwind` intrinsic 的简化形：正常 → dst/target；guest panic → catch_dst/catch_target
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

/// 一个函数体。槽约定：slot 0 = 返回值，1..=num_args = 参数，其余 = 局部/临时。
#[derive(Clone, Debug)]
pub struct Body {
    pub num_slots: u32,
    pub num_args: u32,
    pub blocks: Vec<Block>,
}

/// 一个程序 = 一组函数，按 `FuncId`（下标）互相调用。
#[derive(Clone, Debug)]
pub struct Program {
    pub funcs: Vec<Body>,
}
