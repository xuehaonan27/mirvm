//! x86_64 实现汇总（arch 层唯一架构实现；各子模块契约见各自模块头）。

pub mod asmstub;
pub mod intrinsics;

pub(crate) use intrinsics::*;
