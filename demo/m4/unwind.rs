// M4.2 gate：unwind——panic 发起/传播/Drop-in-unwind/catch/重抛（返回 u64 校验和，
// 无 println——全量差分 M4.3 起）。#[unsafe(no_mangle)] = 收集根 + --vm-call 稳定名。
#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};

static DROPS: AtomicU64 = AtomicU64::new(0);

struct G(u64);
impl Drop for G {
    fn drop(&mut self) {
        DROPS.fetch_add(self.0, Ordering::SeqCst);
    }
}

/// catch_unwind 捕获 + Drop 在 unwind 中执行（奇数 panic：100·1000+10；偶数：7·1000+10）
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

/// 跨多帧传播：中间帧 Drop 在 unwind 中逐帧执行（内层先）
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

/// 越界 Assert → 真 panic_bounds_check → catch（Assert 展开的实证）
#[unsafe(no_mangle)]
pub fn bounds_digest(n: u64) -> u64 {
    let v = [10u64, 20, 30];
    let r = std::panic::catch_unwind(|| v[n as usize]);
    match r {
        Ok(x) => x,
        Err(_) => 999,
    }
}

/// panic 消息 payload 跨 unwind 存活（格式化 String → downcast → 长度）
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

/// 捕获后重抛（resume_unwind）→ 外层再捕获
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
