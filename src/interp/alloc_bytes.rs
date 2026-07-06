//! MirvmAllocBytes：按 guest 要求的对齐做真实宿主分配的 AllocBytes。
//! 移植自 Miri 的 MiriAllocBytes（rust-lang/miri, MIT/Apache-2.0），去掉隔离分配器。
//!
//! 为什么不用 Box<[u8]>：它只保证 1 字节对齐——真实地址模式（账本 C2）下，
//! guest 分配的宿主缓冲必须真对齐（FFI 直传、并行 tier 原子直落、guest 的
//! `ptr.is_aligned()` 判断都依赖它），且大小为 0 时也要有唯一地址。

use std::alloc::{self, Layout};
use std::borrow::Cow;
use std::slice;

use rustc_abi::{Align, Size};
use rustc_middle::mir::interpret::AllocBytes;

#[derive(Debug)]
pub struct MirvmAllocBytes {
    /// 名义布局（size 可为 0）。
    layout: Layout,
    /// 缓冲指针。不变式：size==0 时按 size=1 的等价布局分配（地址唯一）；
    /// 否则按 layout 分配。结构体移动不影响缓冲地址（地址稳定）。
    ptr: *mut u8,
}

impl Clone for MirvmAllocBytes {
    fn clone(&self) -> Self {
        let align = Align::from_bytes(self.layout.align() as u64).unwrap();
        Self::from_bytes(Cow::Borrowed(&**self), align, ())
    }
}

impl Drop for MirvmAllocBytes {
    fn drop(&mut self) {
        let alloc_layout = if self.layout.size() == 0 {
            Layout::from_size_align(1, self.layout.align()).unwrap()
        } else {
            self.layout
        };
        // SAFETY: 不变式保证 ptr 由该布局分配
        unsafe { alloc::dealloc(self.ptr, alloc_layout) }
    }
}

impl std::ops::Deref for MirvmAllocBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr 非空、对齐，可读 layout.size() 字节（size==0 时也成立）
        unsafe { slice::from_raw_parts(self.ptr, self.layout.size()) }
    }
}

impl std::ops::DerefMut for MirvmAllocBytes {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: 同上
        unsafe { slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl MirvmAllocBytes {
    fn alloc_with(
        size: u64,
        align: u64,
        alloc_fn: impl FnOnce(Layout) -> *mut u8,
    ) -> Option<MirvmAllocBytes> {
        let size = usize::try_from(size).ok()?;
        let align = usize::try_from(align).ok()?;
        let layout = Layout::from_size_align(size, align).ok()?;
        // size 0 也分配 1 字节，保证地址唯一
        let alloc_layout =
            if size == 0 { Layout::from_size_align(1, align).unwrap() } else { layout };
        let ptr = alloc_fn(alloc_layout);
        if ptr.is_null() { None } else { Some(MirvmAllocBytes { layout, ptr }) }
    }

    /// 缓冲的宿主地址（真实地址模式的基址来源）。
    pub fn host_addr(&self) -> u64 {
        self.ptr as u64
    }
}

impl AllocBytes for MirvmAllocBytes {
    type AllocParams = ();

    fn from_bytes<'a>(slice: impl Into<Cow<'a, [u8]>>, align: Align, _params: ()) -> Self {
        let slice = slice.into();
        let size = slice.len() as u64;
        // SAFETY: alloc 只在 size!=0 的布局上调用
        let out = Self::alloc_with(size, align.bytes(), |l| unsafe { alloc::alloc(l) })
            .unwrap_or_else(|| panic!("mirvm 内存不足：无法分配 {size} 字节"));
        // SAFETY: 两侧均非空且大小匹配
        unsafe { out.ptr.copy_from(slice.as_ptr(), slice.len()) };
        out
    }

    fn zeroed(size: Size, align: Align, _params: ()) -> Option<Self> {
        Self::alloc_with(size.bytes(), align.bytes(), |l| unsafe { alloc::alloc_zeroed(l) })
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    fn as_ptr(&self) -> *const u8 {
        self.ptr
    }
}
