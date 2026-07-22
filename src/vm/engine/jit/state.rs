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

/// strict 失败哨兵（MIRVM_JIT_SYNC 验证模式）：可准入函数编译失败时
/// worker 写入 slots——SYNC 等待方据此响亮 abort（区别于 0 = 未编译/
/// 维持解释的正常值域；非 strict 模式绝不写入）。
pub const FAIL_SENTINEL: u64 = u64::MAX;

pub struct JitState {
    /// PLT 槽（interp i2c 面）：FuncId → packed 入口机器地址（0 = 未编译，走解释）。
    pub slots: Vec<AtomicU64>,
    /// PLT 槽（编译码 cc→cc 面）：FuncId → fast 入口 / c2i 蹦床地址（0 = 尚无）。
    /// 只有编译码的调用点读它（load + call_indirect）；interp 不消费。
    pub slots_fast: Vec<AtomicU64>,
    /// 调用计数（Relaxed；竞态丢计无害——只影响触发时刻，不影响语义）
    pub counters: Vec<AtomicU32>,
    /// `--jit off` / `MIRVM_JIT=off` ⇒ false：纯解释，计数也不做（对拍口径）
    pub enabled: bool,
    /// 过阈值投递编译队列（Q3 裁定 1000）
    pub threshold: u32,
    /// `MIRVM_JIT_SYNC=1` 验证模式（audit F-05）：投递后等待发布/失败哨兵
    /// ——threshold=1 的语义从「首调请求编译」升为「首调同步编译发布」，
    /// 可准入函数的编译失败从静默留解释升为响亮 abort（gate 显形）
    pub sync: bool,
    /// 编译请求通道（M5.3b：jit_compile::start 装填；cranelift feature 关 = 恒 None）
    pub queue: std::sync::Mutex<Option<std::sync::mpsc::Sender<u32>>>,
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
            slots_fast: (0..fn_count).map(|_| AtomicU64::new(0)).collect(),
            counters: (0..fn_count).map(|_| AtomicU32::new(0)).collect(),
            enabled,
            threshold,
            sync: std::env::var_os("MIRVM_JIT_SYNC").is_some(),
            queue: std::sync::Mutex::new(None),
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
