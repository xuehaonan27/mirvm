//! arch 层（结构重构战役片2）：mirvm 触及**架构相关操作**的唯一通道
//! （与 os/ 层对称的 leaf：不依赖 engine/lower/rustc_private）。
//!
//! # 边界契约
//!
//! - **leaf**：函数签名只出现裸指针/整数/自有小类型——无引擎类型、无 OS
//!   依赖（OS 在 os/ 层，本层不触碰）。
//! - **原语，不裁决**：CPUID 派发（guest 的 feature 选择）、调用时机与
//!   失败语义留引擎；本层只执行真宿主指令/发射真机器码字节。
//! - 接纳面：硬件 intrinsic 执行体（llvm.x86.* 边界后的真身）、机器码
//!   字节发射（stub）、单发指令原语（int3/xgetbv）。
//! - 不接纳：`lower/asm.rs` 的寄存器分配与 llvm.x86 名表（rustc 类型
//!   耦合）——留 lower 域并记档（decision-history §7.16）。
//!
//! # 平台选定
//!
//! 当前唯一实现 = `x86_64/`（与 os/linux、global_asm/asm-stub 硬门同前提）。
//! 非 x86_64 目标直接编译期失败；新增架构 = 平行实现目录 + 此处 cfg 分派
//! （OpenJDK cpu/ 族形态）。

#[cfg(target_arch = "x86_64")]
pub(crate) mod x86_64;

#[cfg(not(target_arch = "x86_64"))]
compile_error!("arch 层当前仅实现 x86_64（与 global_asm/asm-stub 硬门同前提）");
