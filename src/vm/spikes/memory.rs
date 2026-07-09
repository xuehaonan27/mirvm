//! Spike 1b：guest 内存 = **真实地址 + 裸访问**，无 AllocId、无检查器 overlay。
//!
//! 这正是 tier-0 InterpCx 检查器拒绝的模型（corpus §2.5：walkdir/process/mmap
//! 三实例都因"读无 AllocId 的真地址指针"被判 DanglingIntPointer）。M4 甩掉 overlay
//! 后，guest 指针就是真宿主地址、裸 read/write，本文件是它的最小骨架示范。
//!
//! bump 分配、无 free（skeleton；真实现走 TLAB + mimalloc 结构，见 concurrency-arch.md §3.3）。

/// 一块固定大小的匿名映射；返回给 guest 的"指针"就是真宿主地址。
pub struct GuestMemory {
    base: *mut u8,
    size: usize,
    offset: usize,
}

impl GuestMemory {
    pub fn new(size: usize) -> Self {
        // 真地址内存：mmap 匿名 RW。指针交给 guest 后按真地址裸读写——
        // 内核给的地址，mirvm 不登记 AllocId、不做范围检查（fast）。
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert!(base != libc::MAP_FAILED, "GuestMemory: mmap 失败");
        GuestMemory { base: base as *mut u8, size, offset: 0 }
    }

    /// bump 分配 `size` 字节（对齐到 8），返回真地址。
    pub fn alloc(&mut self, size: u64) -> u64 {
        let aligned = (self.offset + 7) & !7;
        let end = aligned + size as usize;
        assert!(end <= self.size, "GuestMemory: 溢出（bump，无 free）");
        self.offset = end;
        (self.base as u64) + aligned as u64
    }

    /// 裸读一个 u64（真地址，无 AllocId 检查）。
    ///
    /// # Safety
    /// `addr` 必须在本区内且已初始化。fast machine 假设 guest 合法（越界=guest UB）。
    pub unsafe fn load(&self, addr: u64) -> u64 {
        unsafe { (addr as *const u64).read_unaligned() }
    }

    /// 裸写一个 u64（真地址，无 AllocId 检查）。
    ///
    /// # Safety
    /// 同 [`load`](Self::load)。
    pub unsafe fn store(&self, addr: u64, val: u64) {
        unsafe { (addr as *mut u64).write_unaligned(val) }
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.size) };
    }
}
