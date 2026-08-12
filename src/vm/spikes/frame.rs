//! slaved 操作数区（Spike 1 原型，模型 A 的帧局部存储）。
//!
//! 所有帧的局部槽切在一条连续 `Vec` 上；每帧 `reserve` 一段、返回时 `restore`。
//! 生产实现后来改为带 guard page 的 mmap ByteRegion；2026-08-12 又撤销了 alloca
//! 必迁承诺，slaved 正式保留。这里仍保持一个**窄接口**，且
//! **不与安全模式（fast/checked）耦合**：Spike 1 是 fast，不做任何范围检查；
//! checked 模式将来只在 `GuestMemory::contains` 处相遇（见账本 C13 解耦要求）。

pub type Word = u64;

/// 一条连续的槽栈。帧按 reserve/restore 后进先出地使用。
#[derive(Default)]
pub struct OperandRegion {
    slots: Vec<Word>,
}

impl OperandRegion {
    pub fn new() -> Self {
        OperandRegion { slots: Vec::new() }
    }

    /// 为一个新帧切出 `n` 个槽（清零），返回基址（切出前的区尾）。
    pub fn reserve(&mut self, n: u32) -> usize {
        let base = self.slots.len();
        self.slots.resize(base + n as usize, 0);
        base
    }

    /// 弹出该帧的槽（截回 `base`）。与 `reserve` 严格配对。
    pub fn restore(&mut self, base: usize) {
        self.slots.truncate(base);
    }

    #[inline]
    pub fn read(&self, base: usize, slot: u32) -> Word {
        self.slots[base + slot as usize]
    }

    #[inline]
    pub fn write(&mut self, base: usize, slot: u32, v: Word) {
        self.slots[base + slot as usize] = v;
    }
}
