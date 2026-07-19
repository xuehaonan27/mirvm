//! os 层（P7 兑现，DESIGN.md P7 / open-issues E21 / decision-history §7.16）：
//! mirvm 触及真 OS 的**唯一通道**。引擎（vm/）与加载相（lower/）的业务代码
//! 只调这里的原语，不再出现 `libc::` / `std::os::unix` 触点。
//!
//! # 边界契约
//!
//! - **leaf**：本层不依赖 engine/lower/rustc_private；函数签名只出现
//!   usize/u64/裸指针/自有小枚举——**无 guest 概念**。
//! - **原语，不裁决**：guest 语义裁决（单线程 fork 守卫、信号 handler 白名单、
//!   sigaction 结构体改拷贝、响亮拒绝文案）全部留在引擎业务侧；本层只做
//!   诚实的 OS 调用与错误回传（OpenJDK `os::` 同款纪律）。
//! - **直通优先**：能直通就不包装（C10：绝不广泛拦截 native 操作）；未列举的
//!   syscall 族经 `os::process::syscall6` 单点变参直通，不为每个 syscall 建壳。
//!
//! # 平台选定
//!
//! 当前唯一实现 = `linux/`（与 `lower/global_asm.rs`、asm-stub 工厂的
//! x86_64 硬门同前提）。非 Linux 目标直接编译期失败——诚实不装可移植；
//! 新增平台 = 平行实现目录 + 此处 cfg 分派（OpenJDK os/ 族形态）。

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;

#[cfg(not(target_os = "linux"))]
compile_error!("os 层当前仅实现 linux（与 global_asm/asm-stub 的 x86_64 硬门同前提）");
