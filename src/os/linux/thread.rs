//! os::thread — Linux pthread 原语：TLS key 族、栈界探测、attr 栈尺寸、
//! `/proc/self/task` 线程计数。
//!
//! 归并：ctx.rs（Ctx TLS key 族 + 栈安全下界的 getattr 部分）、ffi.rs
//! amplify_pthread_stack 的 attr 读写、glibc 未设-stacksize 假地址知识。
//! 原语不裁决：Ctx attach/dtor 语义、fork 基线判定、栈放大的 AMPLIFY/FLOOR
//! 策略、安全边距规则全部留引擎；本层只给诚实的 pthread 读写。
//!
//! 类型纪律：attr 指针以 c_void 出入（libc pthread 类型不外泄签名）。

use std::ffi::c_void;

/// pthread TLS key（创建后进程内唯一；dtor 形态由调用方定）。
#[derive(Clone, Copy)]
pub struct TlsKey(libc::pthread_key_t);

/// pthread_key_create；失败 panic（现存语义：引擎没有无钥匙的降级路径）。
pub fn tls_key_create(dtor: Option<unsafe extern "C" fn(*mut c_void)>) -> TlsKey {
    let mut k: libc::pthread_key_t = 0;
    let rc = unsafe { libc::pthread_key_create(&mut k, dtor) };
    assert_eq!(rc, 0, "pthread_key_create 失败: {rc}");
    TlsKey(k)
}

/// pthread_getspecific。
///
/// # Safety
/// 与原生语义一致；调用方保证读出指针的回收纪律。
pub unsafe fn tls_get(key: TlsKey) -> *mut c_void {
    unsafe { libc::pthread_getspecific(key.0) }
}

/// pthread_setspecific。
///
/// # Safety
/// 与原生语义一致。
pub unsafe fn tls_set(key: TlsKey, p: *mut c_void) {
    unsafe { libc::pthread_setspecific(key.0, p) };
}

/// 本线程栈 [lo, lo+size)（pthread_getattr_np + getstack + destroy）。
/// 主线程 getattr 内部读 /proc——非热路径原语；失败 None（调用方保守处理）。
pub fn current_stack_bounds() -> Option<(usize, usize)> {
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let rc = attr_stack_bounds(&mut attr as *mut _ as *mut c_void);
        libc::pthread_attr_destroy(&mut attr);
        rc
    }
}

/// attr 的栈设置（pthread_attr_getstack 直通）：Some((lo, size))，失败 None。
/// 注意 glibc 未设-stacksize 的假地址形态由 `stack_addr_is_unset` 判定，
/// 调用方不得自行解释 lo。
pub fn attr_stack_bounds(attr: *mut c_void) -> Option<(usize, usize)> {
    let mut lo: *mut libc::c_void = std::ptr::null_mut();
    let mut size: libc::size_t = 0;
    let rc = unsafe { libc::pthread_attr_getstack(attr as *mut libc::pthread_attr_t, &mut lo, &mut size) };
    if rc != 0 || size == 0 {
        return None;
    }
    Some((lo as usize, size))
}

/// glibc 细节：未 setstack 的 attr 内部 stackaddr=NULL，getstack 返回
/// `NULL - stacksize`（近 u64 顶的假地址）而非 NULL。x86_64 用户地址
/// ≤ 47 位——超界即「未设」；真用户栈地址（guest 自供栈）落在界内。
pub fn stack_addr_is_unset(lo: usize) -> bool {
    lo == 0 || lo >= 1 << 48
}

/// pthread_attr_setstacksize；成功 true。
pub fn attr_set_stack_size(attr: *mut c_void, size: usize) -> bool {
    unsafe { libc::pthread_attr_setstacksize(attr as *mut libc::pthread_attr_t, size) == 0 }
}

/// OS 线程数（`/proc/self/task` 目录项计数；fork 守卫基线的唯一可靠来源——
/// pthread_create 返回后新线程即存在，应用层计数器有 TOCTOU 窗口）。
/// 读取失败 0（调用方保守处理）。
pub fn os_thread_count() -> usize {
    std::fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(0)
}
