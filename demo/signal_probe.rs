// 信号差分探针：内核信号帧只登记，guest handler 在 VM 安全点执行。
// `raise` 是当前线程的同步投递；handler 内再 raise 另一信号时，内层
// handler 必须在 raise 返回前完成。TRACE 锁住这个 native 顺序，避免只看最终
// 计数时把 1-3-2 的延迟执行假判为 1-2-3。
// libc 经 extern "C" 直声明（demo 用 plain rustc 编译，无 libc crate）。
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

const SIGUSR1: i32 = 10;
const SIGUSR2: i32 = 12;
const SIG_IGN: usize = 1;

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn raise(sig: i32) -> i32;
    // sigaction 结构布局（x86_64 glibc）：handler 在偏移 0
    fn sigaction(signum: i32, act: *const SigAction, old: *mut SigAction) -> i32;
    fn sigemptyset(set: *mut SigSet) -> i32;
}

#[repr(C)]
struct SigSet {
    bytes: [u8; 128],
}
#[repr(C)]
struct SigAction {
    sa_handler: usize,
    sa_mask: SigSet,
    sa_flags: i32,
    sa_restorer: usize,
}

static USR1: AtomicU32 = AtomicU32::new(0);
static USR2: AtomicU32 = AtomicU32::new(0);
static LAST: AtomicI32 = AtomicI32::new(0);
static TRACE: AtomicU32 = AtomicU32::new(0);

fn trace(digit: u32) {
    TRACE.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
        Some(value * 10 + digit)
    })
    .unwrap();
}

extern "C" fn on_usr1(sig: i32) {
    trace(1);
    USR1.fetch_add(1, Ordering::SeqCst);
    LAST.store(sig, Ordering::SeqCst);
    // 重入：在 USR1 handler 内 raise USR2（run-to-completion 嵌套）
    if USR1.load(Ordering::SeqCst) == 1 {
        unsafe { raise(SIGUSR2) };
    }
    trace(3);
}
extern "C" fn on_usr2(sig: i32) {
    trace(2);
    USR2.fetch_add(1, Ordering::SeqCst);
    LAST.store(sig, Ordering::SeqCst);
}

fn main() {
    unsafe { signal(SIGUSR1, on_usr1 as *const () as usize) };
    unsafe {
        let mut act: SigAction = std::mem::zeroed();
        act.sa_handler = on_usr2 as *const () as usize;
        sigemptyset(&mut act.sa_mask);
        sigaction(SIGUSR2, &act, std::ptr::null_mut());
    }

    unsafe { raise(SIGUSR1) }; // → usr1-before → usr2 → usr1-after
    let nested_trace = TRACE.load(Ordering::SeqCst);
    assert_eq!(nested_trace, 123, "nested raise did not run to completion");
    println!("nested trace = {nested_trace}");
    unsafe { raise(SIGUSR1) }; // → on_usr1（第二次，不再嵌套）
    unsafe { raise(SIGUSR2) }; // → on_usr2 直接

    println!(
        "usr1 = {}, usr2 = {}, last = {}",
        USR1.load(Ordering::SeqCst),
        USR2.load(Ordering::SeqCst),
        LAST.load(Ordering::SeqCst)
    );

    unsafe { signal(SIGUSR1, SIG_IGN) };
    unsafe { raise(SIGUSR1) };
    println!("after SIG_IGN usr1 = {}", USR1.load(Ordering::SeqCst));
}
