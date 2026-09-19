// Permanent differential probe for atomic ordering. Two kinds of oracle:
// 1. Combination matrix: every operation x every legal ordering is executed for real, so a
//    mis-mapped ordering (e.g. load(Release)) trips the host atomic API panic immediately;
// 2. Acquire/Release message passing: data published by a release-store must be visible once
//    an acquire-load sees the flag (scheduling is nondeterministic; the invariant is asserted).
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
        fence(o); // there is no Relaxed fence (std panics)
    }
    let _ = a.compare_exchange(7, 9, Ordering::AcqRel, Ordering::Acquire);
    let _ = a.compare_exchange(9, 11, Ordering::Release, Ordering::Relaxed);
    let _ = a.compare_exchange_weak(11, 13, Ordering::SeqCst, Ordering::SeqCst);
    let _ = a.compare_exchange(999, 0, Ordering::Relaxed, Ordering::Relaxed); // must-fail path
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
    // An acquire load seeing the flag => writes before the release store must be visible (happens-before)
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
