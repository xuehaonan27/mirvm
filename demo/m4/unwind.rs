// Unwind probe: panic raise / propagation / Drop-in-unwind / catch / rethrow, each returning
// a u64 checksum instead of printing. #[unsafe(no_mangle)] = mono collection root + stable --vm-call name.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static DROPS: AtomicU64 = AtomicU64::new(0);
static TOP_PAYLOAD_DROPS: AtomicUsize = AtomicUsize::new(0);
static SECOND_PAYLOAD_DROPS: AtomicUsize = AtomicUsize::new(0);

struct G(u64);
impl Drop for G {
    fn drop(&mut self) {
        DROPS.fetch_add(self.0, Ordering::SeqCst);
    }
}

struct SecondTopPayload;

impl Drop for SecondTopPayload {
    fn drop(&mut self) {
        let count = SECOND_PAYLOAD_DROPS.fetch_add(1, Ordering::SeqCst) + 1;
        println!(
            "second-payload-drop={count} panicking={}",
            std::thread::panicking()
        );
    }
}

struct TopPayload;

impl Drop for TopPayload {
    fn drop(&mut self) {
        let count = TOP_PAYLOAD_DROPS.fetch_add(1, Ordering::SeqCst) + 1;
        println!(
            "top-payload-drop={count} panicking={}",
            std::thread::panicking()
        );
        println!("normal-after-top-cleanup={}", normal_after_top_cleanup(40));

        let second = std::panic::catch_unwind(|| {
            std::panic::panic_any(SecondTopPayload);
        });
        println!(
            "second-panic-caught={} panicking={}",
            second.is_err(),
            std::thread::panicking()
        );
        drop(second);
        println!(
            "payload-drop-counts={}:{} panicking={}",
            TOP_PAYLOAD_DROPS.load(Ordering::SeqCst),
            SECOND_PAYLOAD_DROPS.load(Ordering::SeqCst),
            std::thread::panicking()
        );
    }
}

#[inline(never)]
fn normal_after_top_cleanup(value: u64) -> u64 {
    value + 2
}

/// A panic caught at the engine top level must be handed back to guest std to reset counters and drop the payload.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn uncaught_payload_cleanup_probe() -> u64 {
    TOP_PAYLOAD_DROPS.store(0, Ordering::SeqCst);
    SECOND_PAYLOAD_DROPS.store(0, Ordering::SeqCst);
    std::panic::panic_any(TopPayload)
}

/// catch_unwind catches it and Drop runs during unwinding (odd panic: 100*1000+10; even: 7*1000+10)
#[unsafe(no_mangle)]
pub fn catch_digest(n: u64) -> u64 {
    DROPS.store(0, Ordering::SeqCst);
    let _outer = G(1);
    let r = std::panic::catch_unwind(|| {
        let _inner = G(10);
        if n % 2 == 1 {
            panic!("odd");
        }
        7u64
    });
    let caught = match r {
        Ok(v) => v,
        Err(_) => 100,
    };
    caught * 1000 + DROPS.load(Ordering::SeqCst)
}

fn middle(n: u64) -> u64 {
    let _mid = G(10);
    if n % 2 == 1 {
        panic!("deep");
    }
    5
}

/// Propagation across frames: the middle frame's Drop runs frame by frame during unwinding (inner first)
#[unsafe(no_mangle)]
pub fn nested_digest(n: u64) -> u64 {
    DROPS.store(0, Ordering::SeqCst);
    let r = std::panic::catch_unwind(|| {
        let _outer = G(100);
        middle(n)
    });
    let caught = match r {
        Ok(v) => v,
        Err(_) => 77,
    };
    caught * 1000 + DROPS.load(Ordering::SeqCst)
}

/// Out-of-bounds Assert -> real panic_bounds_check -> catch (evidence that Assert expands)
#[unsafe(no_mangle)]
pub fn bounds_digest(n: u64) -> u64 {
    let v = [10u64, 20, 30];
    let r = std::panic::catch_unwind(|| v[n as usize]);
    match r {
        Ok(x) => x,
        Err(_) => 999,
    }
}

/// The panic message payload survives unwinding (formatted String -> downcast -> length)
#[unsafe(no_mangle)]
pub fn msg_digest(n: u64) -> u64 {
    let r = std::panic::catch_unwind(move || {
        if n > 0 {
            panic!("code-{n}");
        }
        0u64
    });
    match r {
        Ok(v) => v,
        Err(e) => match e.downcast_ref::<String>() {
            Some(s) => s.len() as u64 * 10 + 1,
            None => 2,
        },
    }
}

/// Rethrow after catching (resume_unwind) -> caught again by the outer frame
#[unsafe(no_mangle)]
pub fn rethrow_digest(n: u64) -> u64 {
    DROPS.store(0, Ordering::SeqCst);
    let r = std::panic::catch_unwind(|| {
        let _g = G(3);
        let inner = std::panic::catch_unwind(|| {
            if n % 2 == 1 {
                panic!("boom");
            }
            11u64
        });
        match inner {
            Ok(v) => v,
            Err(e) => std::panic::resume_unwind(e),
        }
    });
    let caught = match r {
        Ok(v) => v,
        Err(_) => 55,
    };
    caught * 100 + DROPS.load(Ordering::SeqCst)
}

fn main() {}
