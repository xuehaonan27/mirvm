//! `vm` — 模型 A 执行相（**纯 Rust，零 rustc_private**——机械门禁 = tsan harness
//! 以 `#[path]` 同源编译本模块，漏进 rustc 类型即编译失败）。
//!
//! - `engine/`：M4 真引擎（类型化字节码 IR / place 求值 / FrameGuard unwind /
//!   mimalloc 堆 / 冻结区 / dlsym+libffi 直通）。
//! - `spikes/`：M4 前置验证的**冻结工件**（模型 A 骨架、i2c/c2i、混合栈 unwind、
//!   并发 TSan、真 Cranelift）——回归自检用（`mirvm spike1..5`），勿动勿扩展；
//!   经验见 docs/spike{1..5}-*.md。

pub mod engine;
pub mod spikes;
