// M5.2 D8j atomic 序贯通的永久差分探针。两类 oracle：
// ① 组合矩阵：每种操作 × 每种合法序都真实执行——引擎映射错序（如 load(Release)）
//    会被宿主原子 API panic 当场抓住；
// ② Acquire/Release 消息传递不变式：release-store 发布的数据，acquire-load 看到
//    flag 后必须可见（弱序语义的可观测面；调度非确定，断言的是不变式而非交错）。
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};

fn order_matrix() {
    let a = AtomicU64::new(1);
    for o in [Ordering::Relaxed, Ordering::Acquire, Ordering::SeqCst] {
        let _ = a.load(o);
    }
    for o in [Ordering::Relaxed, Ordering::Release, Ordering::SeqCst] {
        a.store(2, o);
    }
    for o in [
        Ordering::Relaxed,
        Ordering::Acquire,
        Ordering::Release,
        Ordering::AcqRel,
        Ordering::SeqCst,
    ] {
        a.fetch_add(1, o);
        a.fetch_max(3, o);
        a.swap(7, o);
    }
    for o in [
        Ordering::Acquire,
        Ordering::Release,
        Ordering::AcqRel,
        Ordering::SeqCst,
    ] {
        fence(o); // Relaxed fence 不存在（std panic）
    }
    let _ = a.compare_exchange(7, 9, Ordering::AcqRel, Ordering::Acquire);
    let _ = a.compare_exchange(9, 11, Ordering::Release, Ordering::Relaxed);
    let _ = a.compare_exchange_weak(11, 13, Ordering::SeqCst, Ordering::SeqCst);
    let _ = a.compare_exchange(999, 0, Ordering::Relaxed, Ordering::Relaxed); // 必败路
    println!("matrix final = {}", a.load(Ordering::Relaxed));
}

fn message_passing() {
    static DATA: AtomicUsize = AtomicUsize::new(0);
    static FLAG: AtomicBool = AtomicBool::new(false);
    let producer = std::thread::spawn(|| {
        DATA.store(42, Ordering::Relaxed);
        FLAG.store(true, Ordering::Release);
    });
    while !FLAG.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    // Acquire 看到 flag ⇒ Release 之前的写必须可见（happens-before 不变式）
    println!("mp data = {}", DATA.load(Ordering::Relaxed));
    producer.join().unwrap();
}

fn contended_relaxed() {
    static C: AtomicU32 = AtomicU32::new(0);
    let hs: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..10_000 {
                    C.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    println!("relaxed count = {}", C.load(Ordering::Relaxed));
}

fn main() {
    order_matrix();
    message_passing();
    contended_relaxed();
}
