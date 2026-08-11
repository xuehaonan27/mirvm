//! os::mem — Linux 匿名映射原语（mmap/mprotect/munmap + 偏好固定基址）。
//!
//! 引擎所有匿名映射的唯一通道：frozen（冻结区）、codearena（stub 码域）、
//! frame（字节区帧）三处 mmap 形态的归并。只出现 usize/裸指针/Prot——
//! 无 guest 概念；容量、基址数值（addrlayout）与耗尽文案都是调用方的事。
//!
//! 失败语义（与归并前三处形态逐字对齐，调用方各自决定措辞）：
//! - 动态映射失败 → 空指针（调用方 assert/panic）。
//! - 偏好固定基址被占或失败 → Ok(None)（调用方决定动态回退/响亮报错；
//!   MAP_FIXED_NOREPLACE 绝不覆盖既有映射）。

/// mmap 保护标志的窄枚举（调用点不再散落 libc 常量）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prot(std::os::raw::c_int);

impl Prot {
    /// No access. Used for guard pages around guest-owned mappings.
    pub const NONE: Prot = Prot(libc::PROT_NONE);
    /// PROT_READ | PROT_WRITE
    pub const RW: Prot = Prot(libc::PROT_READ | libc::PROT_WRITE);
    /// PROT_READ | PROT_EXEC
    pub const RX: Prot = Prot(libc::PROT_READ | libc::PROT_EXEC);
}

pub fn page_size() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(n > 0, "sysconf(_SC_PAGESIZE) failed");
    n as usize
}

/// 匿名私有动态映射；`noreserve` = 虚拟保留不占提交（frame 1GiB 区形态）。
/// 失败返回空指针（与归并前 `!= MAP_FAILED` 判式一致，措辞归调用方）。
pub fn map_anon(size: usize, prot: Prot, noreserve: bool) -> *mut u8 {
    let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    if noreserve {
        flags |= libc::MAP_NORESERVE;
    }
    let p = unsafe { libc::mmap(std::ptr::null_mut(), size, prot.0, flags, -1, 0) };
    if p == libc::MAP_FAILED {
        std::ptr::null_mut()
    } else {
        p as *mut u8
    }
}

/// 偏好固定基址映射（MAP_FIXED_NOREPLACE）：成功 Some，被占/失败 None。
/// 引擎可缓存性地基（JVM CDS 同思路；错基址 = 静默错值，故绝不覆盖既有映射）。
pub fn map_fixed_preferred(addr: usize, size: usize, prot: Prot) -> Option<*mut u8> {
    let p = unsafe {
        libc::mmap(
            addr as *mut libc::c_void,
            size,
            prot.0,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED_NOREPLACE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return None;
    }
    Some(p as *mut u8)
}

/// mprotect 窄封装（codearena 填完封存 RX 的 W^X 形态）。
pub fn protect(addr: *mut u8, size: usize, prot: Prot) -> Result<(), String> {
    let rc = unsafe { libc::mprotect(addr as *mut libc::c_void, size, prot.0) };
    if rc != 0 {
        return Err(format!("mprotect({addr:p}, {size:#x}) 失败 rc={rc}"));
    }
    Ok(())
}

/// munmap（调用方持容量与生命周期）。
///
/// # Safety
/// addr/size 必须出自本层一次成功映射的同一区间；调用方保证之后不再触碰。
pub unsafe fn unmap(addr: *mut u8, size: usize) {
    unsafe { libc::munmap(addr as *mut libc::c_void, size) };
}
