//! os::dll — Linux 动态装载原语：dlopen/dlsym/dlerror/dlinfo 装载基址。
//!
//! 归并：ffi.rs（ensure_libs 的 dlopen 两档 + 各级 dlsym）、lower/mod.rs
//! （weak 缺席探测与必需库预载）、elfsym.rs（dlinfo 取 l_addr）。
//! 绑定优先序（归档兜底→必需句柄→全域→可选句柄）与「符号不存在」的业务
//! 语义留调用方；本层只供单发原语。句柄按 usize 出入（leaf 类型纪律）。

use std::ffi::CStr;

/// dlopen 档位（全部调用点恒带 RTLD_GLOBAL，固化为内部常量）。
#[derive(Clone, Copy)]
pub enum Mode {
    Now,
    Lazy,
}

/// dlopen：成功 = 句柄（usize，非 0）；失败 = Err（dlerror 详情——粘滞
/// 线程局部状态，本函数内先清空再复制，调用方拿到的即是本失败文案）。
pub fn open(path: &CStr, mode: Mode) -> Result<usize, String> {
    let flag = match mode {
        Mode::Now => libc::RTLD_NOW,
        Mode::Lazy => libc::RTLD_LAZY,
    } | libc::RTLD_GLOBAL;
    open_with_flags(path, flag)
}

/// dlopen 显式旗形态（测试/局部可见性用；产品路径用 `open`）。
pub fn open_with_flags(path: &CStr, flag: i32) -> Result<usize, String> {
    unsafe { libc::dlerror() }; // 清空粘滞错误
    let h = unsafe { libc::dlopen(path.as_ptr(), flag) };
    if h.is_null() {
        return Err(error_string());
    }
    Ok(h as usize)
}

/// dlopen 常用旗常量（`open_with_flags` 的组合素材）。
pub const RTLD_NOW: i32 = libc::RTLD_NOW;
pub const RTLD_LOCAL: i32 = libc::RTLD_LOCAL;

/// dlclose。进程级 native 库句柄仍由其调用方有意常驻；Engine 私有且地址不逃逸的
/// 符号镜像在 Module 析构时用本入口配对释放。
///
/// # Safety
/// handle 必须出自本层 open 且之后不再 dlsym。
pub unsafe fn close(handle: usize) {
    unsafe { libc::dlclose(handle as *mut libc::c_void) };
}

/// dlsym 单发：命中 = 地址（非 0），未命中 = 0。handle = 0 表示全域
/// （RTLD_DEFAULT 语义；glibc 下 dlsym(NULL, ...)）。
pub fn sym(handle: usize, name: &CStr) -> usize {
    unsafe { libc::dlsym(handle as *mut libc::c_void, name.as_ptr()) as usize }
}

/// dlerror 当前值（无 = 固定兜底文案；先读先清是调用方节奏——
/// 仅 `open` 内部使用时序正确，外露仅供特殊调用点）。
pub fn error_string() -> String {
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "dlerror 未提供详情".into()
    } else {
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

// glibc link_map（dlinfo RTLD_DI_LINKMAP 返回）的最小字段布局。
#[repr(C)]
struct LinkMap {
    l_addr: usize,
    l_name: *const std::ffi::c_char,
    l_ld: *mut std::ffi::c_void,
    l_next: *mut LinkMap,
    l_prev: *mut LinkMap,
}

/// dlopen 句柄的装载基址（dlinfo → link_map.l_addr）。None = dlinfo 失败
/// （句柄非法——刚 dlopen 的句柄不应发生）。
pub fn load_bias(handle: usize) -> Option<usize> {
    let mut lm: *mut LinkMap = std::ptr::null_mut();
    let r = unsafe {
        libc::dlinfo(
            handle as *mut libc::c_void,
            libc::RTLD_DI_LINKMAP,
            &mut lm as *mut *mut LinkMap as *mut libc::c_void,
        )
    };
    if r == 0 && !lm.is_null() {
        Some(unsafe { (*lm).l_addr })
    } else {
        None
    }
}
