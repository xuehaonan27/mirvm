//! 自研执行引擎（M1: fast Machine on rustc InterpCx；M4 换自研字节码 VM）。
//!
//! 结构大量参考 Miri（MIT/Apache-2.0，rust-lang/miri）——它是 InterpCx/Machine
//! 的参考实现；差异：mirvm 关闭全部 UB 检查（对齐/有效性/借用/数据竞争），
//! 假设程序合法，只追求跑得对、跑得快。

pub mod addrs;
pub mod eval;
pub mod helpers;
pub mod intrinsics;
pub mod machine;
mod mono_map;
pub mod shims;

use rustc_const_eval::interpret::InterpCx;

pub use self::machine::{MirvmMachine, Prov, Termination};

pub type MirvmInterpCx<'tcx> = InterpCx<'tcx, MirvmMachine<'tcx>>;

/// foreign fn / intrinsic shim 的处理结果（Miri 同款语义）。
pub enum EmulateItemResult {
    /// 已写好返回值，需要跳到 ret block
    NeedsReturn,
    /// 需要开始 unwinding（跳到 unwind block）
    NeedsUnwind,
    /// shim 已自行安排好控制流（如压了新栈帧）
    AlreadyJumped,
    /// 不认识该符号
    NotSupported,
}
