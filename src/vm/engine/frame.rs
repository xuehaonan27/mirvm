//! 字节区帧存储：mmap 定容真地址区（M4.1 F6——帧地址终身稳定）。
//!
//! `Ref`/`RawPtr` 取的是**帧内局部的真地址**（&arr、buf.as_mut_ptr()），运行中区不得
//! 搬家 → 定容 mmap（GuestMemory 同款）；匿名映射惰性提交，虚拟保留大、RSS 按触碰页算。
//! 到界 = guest 栈溢出近似（frame-abi §9）：诊断退出（M4.2 起可换真 panic/abort 语义）。
//!
//! 帧 = 按 (frame_size, frame_align) 切一段；局部 = 段内冻结偏移处的字节。
//! 接口保持窄（C13 解耦纪律）：reserve/restore/read/write——**base 现在是真地址**，
//! 帧内/堆上/statics 由此统一为裸地址读写（place 求值的地基）。

use super::ir::{Slot, Width};

/// 每区容量（虚拟保留 64 MiB；M4.4 起每 guest 线程一个区）。
const REGION_CAP: usize = 64 << 20;

pub struct ByteRegion {
    base: *mut u8,
    /// 区内 bump 水位（相对 base 的字节数）
    sp: usize,
}

impl Default for ByteRegion {
    fn default() -> Self {
        Self::new()
    }
}

impl ByteRegion {
    pub fn new() -> Self {
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                REGION_CAP,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert!(base != libc::MAP_FAILED, "ByteRegion: mmap 失败");
        ByteRegion { base: base as *mut u8, sp: 0 }
    }

    /// 为新帧切 `size` 字节（按 `align` 对齐、清零），返回**帧基址（真地址）**。
    /// 与 `restore` 严格配对（slaved 于 interp_frame 递归）。
    pub fn reserve(&mut self, size: u32, align: u32) -> usize {
        let align = align.max(1) as usize;
        // mmap base 页对齐 ⇒ 在绝对地址上对齐即可
        let aligned = (self.base as usize + self.sp + align - 1) & !(align - 1);
        let start = aligned - self.base as usize;
        let end = start + size as usize;
        if end > REGION_CAP {
            eprintln!(
                "mirvm[m4-engine]: guest 栈溢出（操作数区 {} MiB 耗尽）",
                REGION_CAP >> 20
            );
            std::process::exit(70);
        }
        unsafe { std::ptr::write_bytes(self.base.add(start), 0, size as usize) };
        self.sp = end;
        aligned
    }

    pub fn restore(&mut self, base: usize) {
        debug_assert!(base >= self.base as usize && base <= self.base as usize + self.sp);
        self.sp = base - self.base as usize;
    }

    /// 按宽度零扩展读一个标量（base = 帧真地址）。
    #[inline]
    pub fn read(&self, base: usize, slot: Slot) -> u64 {
        let p = (base + slot.off as usize) as *const u8;
        // 帧偏移按布局对齐构造；read_unaligned 起步（正确优先，优化后置）
        unsafe {
            match slot.width {
                Width::W8 => p.read_unaligned() as u64,
                Width::W16 => (p as *const u16).read_unaligned() as u64,
                Width::W32 => (p as *const u32).read_unaligned() as u64,
                Width::W64 => (p as *const u64).read_unaligned(),
            }
        }
    }

    /// 按宽度截断写一个标量（base = 帧真地址）。
    #[inline]
    pub fn write(&mut self, base: usize, slot: Slot, v: u64) {
        let p = (base + slot.off as usize) as *mut u8;
        unsafe {
            match slot.width {
                Width::W8 => p.write_unaligned(v as u8),
                Width::W16 => (p as *mut u16).write_unaligned(v as u16),
                Width::W32 => (p as *mut u32).write_unaligned(v as u32),
                Width::W64 => (p as *mut u64).write_unaligned(v),
            }
        }
    }
}

impl Drop for ByteRegion {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, REGION_CAP) };
    }
}
