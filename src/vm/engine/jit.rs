//! J1 共享基座（M5.3a，m5.3-design §2）：per-fn 分层状态 = PLT 槽表 + 调用计数。
//!
//! 状态格 = Bytecode →（计数过阈值，M5.3b 编译）→ Machine：槽 0 = 解释执行；
//! 非零 = packed 入口机器地址（i2c 直调）。发布协议只有一次原子指针交换——
//! 编译线程 Release 写、call_guest Acquire 读（D4）。
//!
//! 本模块 **feature-free**（纯 std 原子；TSan harness 经 #[path] 同源编译 src/vm，
//! cranelift 只门控 M5.3b 的编译服务）。表按 S4 合并后 FuncId 空间建（base 函数
//! 同等 tier-up，m5.3-design §4）。

use std::sync::atomic::{AtomicU32, AtomicU64};

pub struct JitState {
    /// PLT 槽：FuncId → packed 入口机器地址（0 = 未编译，走解释）。
    pub slots: Vec<AtomicU64>,
    /// 调用计数（Relaxed；竞态丢计无害——只影响触发时刻，不影响语义）
    pub counters: Vec<AtomicU32>,
    /// `--jit off` / `MIRVM_JIT=off` ⇒ false：纯解释，计数也不做（对拍口径）
    pub enabled: bool,
    /// 过阈值投递编译队列（M5.3b 接线；Q3 裁定 1000）
    pub threshold: u32,
}

impl JitState {
    pub fn new(fn_count: usize) -> Self {
        let enabled = match std::env::var("MIRVM_JIT") {
            Ok(v) => !(v == "off" || v == "0"),
            Err(_) => true, // D4：默认 on
        };
        let threshold = std::env::var("MIRVM_JIT_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&t| t > 0)
            .unwrap_or(1000);
        JitState {
            slots: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            counters: (0..fn_count).map(|_| AtomicU32::new(0)).collect(),
            enabled,
            threshold,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::JitState;
    use std::sync::atomic::Ordering;

    #[test]
    fn tables_sized_to_fn_count_and_zero_initialized() {
        let j = JitState::new(7);
        assert_eq!(j.slots.len(), 7);
        assert_eq!(j.counters.len(), 7);
        assert!(j.slots.iter().all(|s| s.load(Ordering::Acquire) == 0));
        assert!(j.threshold > 0);
    }
}
