//! 冻结区：statics / 常量池 / fn-ptr 条目的真地址存储（M4.1 第 4 步）。
//!
//! 加载相物化（两遍法：先分后填破指针环，见 lower），发布后随 Module 只读共享——
//! 例外是 static mut / 内部可变性：guest 可写（真实地址裸写，引擎不经手）。
//! mmap RW 定容（GuestMemory/ByteRegion 同款）：地址终身稳定，重定位一次成真。

/// 冻结区容量（虚拟保留；触碰才占物理页）。
const FROZEN_CAP: usize = 256 << 20;

pub struct FrozenArena {
    base: *mut u8,
    used: usize,
}

impl Default for FrozenArena {
    fn default() -> Self {
        Self::new()
    }
}

impl FrozenArena {
    pub fn new() -> Self {
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                FROZEN_CAP,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert!(base != libc::MAP_FAILED, "FrozenArena: mmap 失败");
        FrozenArena {
            base: base as *mut u8,
            used: 0,
        }
    }

    /// bump 分配（按 align 对齐、清零），返回真地址。
    pub fn alloc(&mut self, size: u64, align: u64) -> u64 {
        let align = align.max(1) as usize;
        let aligned = (self.base as usize + self.used + align - 1) & !(align - 1);
        let start = aligned - self.base as usize;
        let end = start + size as usize;
        assert!(
            end <= FROZEN_CAP,
            "FrozenArena: 冻结区耗尽（{} MiB）",
            FROZEN_CAP >> 20
        );
        self.used = end;
        aligned as u64
    }
}

impl Drop for FrozenArena {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, FROZEN_CAP) };
    }
}

impl std::fmt::Debug for FrozenArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FrozenArena {{ base: {:p}, used: {} }}",
            self.base, self.used
        )
    }
}

// SAFETY: 发布后只读（static mut 的 guest 写走裸地址，不经 &self）；
// 区随 Module 生命周期共享给各执行线程（C8 发布后只读纪律）。
unsafe impl Send for FrozenArena {}
unsafe impl Sync for FrozenArena {}
