//! 字节区帧存储（slaved v0 的 M4 版：字节 arena + 冻结帧布局）。
//!
//! 帧 = 按 (frame_size, frame_align) 切一段；局部 = 段内冻结偏移处的字节。
//! 聚合天然落位（spike1 "真值必须带类型/尺寸"教训的落地）。
//! alloca 迁移后置（C13 解耦纪律：接口保持窄——reserve/restore/read/write）。

use super::ir::{Slot, Width};

pub struct ByteRegion {
    buf: Vec<u8>,
}

impl Default for ByteRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl ByteRegion {
    pub fn new() -> Self {
        ByteRegion { buf: Vec::with_capacity(1 << 16) }
    }

    /// 为新帧切 `size` 字节（按 `align` 对齐、清零），返回基址。与 `restore` 严格配对。
    pub fn reserve(&mut self, size: u32, align: u32) -> usize {
        let align = align.max(1) as usize;
        let base = (self.buf.len() + align - 1) & !(align - 1);
        self.buf.resize(base + size as usize, 0);
        base
    }

    pub fn restore(&mut self, base: usize) {
        self.buf.truncate(base);
    }

    /// 按宽度零扩展读一个标量。
    #[inline]
    pub fn read(&self, base: usize, slot: Slot) -> u64 {
        let p = &self.buf[base + slot.off as usize] as *const u8;
        // 帧偏移按布局对齐构造；read_unaligned 起步（正确优先，优化后置）
        unsafe {
            match slot.width {
                Width::W8 => (p as *const u8).read_unaligned() as u64,
                Width::W16 => (p as *const u16).read_unaligned() as u64,
                Width::W32 => (p as *const u32).read_unaligned() as u64,
                Width::W64 => (p as *const u64).read_unaligned(),
            }
        }
    }

    /// 按宽度截断写一个标量。
    #[inline]
    pub fn write(&mut self, base: usize, slot: Slot, v: u64) {
        let p = &mut self.buf[base + slot.off as usize] as *mut u8;
        unsafe {
            match slot.width {
                Width::W8 => (p as *mut u8).write_unaligned(v as u8),
                Width::W16 => (p as *mut u16).write_unaligned(v as u16),
                Width::W32 => (p as *mut u32).write_unaligned(v as u32),
                Width::W64 => (p as *mut u64).write_unaligned(v),
            }
        }
    }
}
