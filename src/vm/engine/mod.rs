//! M4 引擎（真身）：类型化字节码 + 字节区帧 + 类型化 interp_frame。
//! 纯 Rust、零 rustc_private——tsan harness 同源编译 = 执行相纯度的机械门禁。
//! 加载相（MIR→本 IR 的降低）在 src/lower/（rustc_private 域），产物经 `ir::Module` 交接。

pub mod ctx;
pub mod ffi;
pub mod frame;
pub mod frozen;
pub mod heap;
pub mod interp;
pub mod ir;
pub mod stats;
pub mod thunks;
pub mod tsan_mt;
#[cfg(target_arch = "x86_64")]
pub mod x86;
