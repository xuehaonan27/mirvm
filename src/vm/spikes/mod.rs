//! M4 前置 spike 的冻结工件（2026-07-07 全部验证通过，风险排除即封存）。
//!
//! - `spike1`：最小模型 A 骨架（tree-walking interp_frame + slaved 操作数区 + 真地址）
//! - `spike2`：i2c/c2i 混合栈适配（raw-ptr vmctx 的由来）
//! - `spike3`：混合栈 unwind（候选 A 坐实——M4.2 FrameGuard 协议的原型）
//! - `spike4`：并发 TSan（引擎 Sync 判定；`runtime.tsan` 是门禁载体）
//! - `spike5`：真 Cranelift 接入（P vs R 数据、eh_frame 自注册——M5 检查点）
//! - `bytecode`/`frame`/`interp`/`memory`：spike 专用的冻结基础设施（非 M4 真身，
//!   真身在 ../engine/）。
//!
//! 仅作回归自检（`mirvm spike1..5`）与代码参考，不再扩展。

pub mod bytecode;
pub mod frame;
pub mod interp;
pub mod memory;
pub mod spike1;
pub mod spike2;
pub mod spike3;
pub mod spike4;
#[cfg(feature = "cranelift")]
pub mod spike5;
