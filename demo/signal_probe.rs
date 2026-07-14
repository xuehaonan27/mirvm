// M5.2 D8d 永久差分探针：async 信号 guest handler 经 AS-trampoline 直执行，
// 与 native 同机对拍。覆盖 signal() 与 sigaction() 两条注册路、多信号、计数、
// 以及**重入**（handler 内再 raise 另一信号——设计标注的最尖风险）。
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

extern "C" fn on_usr1(sig: i32) {
    USR1.fetch_add(1, Ordering::SeqCst);
    LAST.store(sig, Ordering::SeqCst);
    // 重入：在 USR1 handler 内 raise USR2（run-to-completion 嵌套）
    if USR1.load(Ordering::SeqCst) == 1 {
        unsafe { raise(SIGUSR2) };
    }
}
extern "C" fn on_usr2(sig: i32) {
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

    unsafe { raise(SIGUSR1) }; // → on_usr1 → 内部 raise USR2 → on_usr2
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
