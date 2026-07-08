//! M4 引擎字节码 IR（类型化；纯 Rust，零 rustc 类型——完全自包含的冻结产物）。
//!
//! 核心决定（M4.0 设计 §1）：**Place 在 lower 期溶解为帧内偏移**。布局全部冻结后，
//! 局部变量身份消失，语句只在 (frame_offset, width) 之间搬运/运算——执行期零符号表查询。
//! local 序号/名字只进诊断串。
//!
//! 与 spike bytecode（../bytecode.rs）分离：spikes 是冻结的验证工件，本 IR 是 M4 真身。

pub type Bb = u32;
pub type FuncId = u32;

/// 标量宽度。W128/浮点 = M4.1（lower 以 Trap 占位）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// 帧内标量槽：冻结偏移（Field 投影已折进 off）。
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub off: u32,
    pub width: Width,
}

#[derive(Clone, Copy, Debug)]
pub enum Operand {
    Slot(Slot),
    Imm { bits: u64, width: Width },
}

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug)]
pub enum IntCc {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug)]
pub enum OvfOp {
    Add,
    Sub,
    Mul,
}

#[derive(Clone, Debug)]
pub enum Rvalue {
    Use(Operand),
    IntBin { op: IntBinOp, signed: bool, a: Operand, b: Operand },
    /// → bool（W8）
    IntCmp { cc: IntCc, signed: bool, a: Operand, b: Operand },
    /// 按位取反（掩到宽度）
    NotBits(Operand),
    /// 逻辑取反（bool：xor 1）——rustc 对 bool 的 Not 语义
    NotBool(Operand),
    /// 二补数取负
    Neg(Operand),
    /// IntToInt：截断后按 from 的符号扩展到 to
    Cast { from: (Width, bool), to: Width, a: Operand },
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Assign {
        dst: Slot,
        rv: Rvalue,
    },
    /// *WithOverflow：一次写 (值槽, 溢出旗标槽)——MIR 的 (T,bool) 标量对，
    /// 两个 dst 的 off 来自冻结的 pair 布局（.0/.1 的 Field 偏移）。
    AssignOverflow {
        op: OvfOp,
        signed: bool,
        a: Operand,
        b: Operand,
        dst_val: Slot,
        dst_flag: Slot,
    },
    /// 语句级 Trap 占位：执行到即诊断退出，但**块的终止子照常降低**——
    /// 保住 Call 边，使 --vm-stats 的可达分析准确（仪器盲点修复）。
    Trap(Box<str>),
    Nop,
}

/// unwind 处置。M4.0 只记录（全 Continue 语义）；M4.2 接 CleanupGuard（spike3 协议原位可插）。
#[derive(Clone, Copy, Debug)]
pub enum UnwindAction {
    Continue,
    Cleanup(Bb),
}

#[derive(Clone, Debug)]
pub enum Terminator {
    Goto(Bb),
    SwitchInt {
        discr: Operand,
        targets: Vec<(u128, Bb)>,
        otherwise: Bb,
    },
    Call {
        callee: FuncId,
        args: Vec<Operand>,
        ret: Option<Slot>,
        target: Bb,
        unwind: UnwindAction,
    },
    /// M4.0：失败 = 引擎 abort 带诊断（M4.2 变真 panic + unwind）
    Assert {
        cond: Operand,
        expected: bool,
        msg: Box<str>,
        target: Bb,
        unwind: UnwindAction,
    },
    Return,
    Unreachable,
    /// ★ Trap-stub：未支持构造的占位（M4 增量协议的核心机制）。
    /// lowering 对收集全集是全量的——不认识的构造绝不中止，就地降为 Trap；
    /// 只有被执行到的路径必须 trap-free。诊断串指出"哪一期欠的账"。
    Trap(Box<str>),
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub term: Terminator,
}

#[derive(Clone, Debug)]
pub struct FuncBody {
    pub frame_size: u32,
    pub frame_align: u32,
    /// 返回槽（_0）；ZST 返回 = None
    pub ret: Option<Slot>,
    /// 参数槽（_1..=_argc 中的标量参；ZST 参已剔除但占位序保留见 lower）
    pub params: Vec<Option<Slot>>,
    pub blocks: Vec<Block>,
    /// 诊断用（符号名）
    pub name: Box<str>,
}

#[derive(Debug, Default)]
pub struct Module {
    pub funcs: Vec<FuncBody>,
    /// 导出名（no_mangle 符号）→ FuncId，--vm-call 查找用
    pub exports: std::collections::HashMap<Box<str>, FuncId>,
}
