#!/usr/bin/env mirvm
---
[dependencies]
libc = "0.2"
---
// 信号处理：注册 guest 处理函数 + raise。handler 是解释代码（无机器地址），
// 内核信号投递需要真机器地址 → 需 thunk（同 pthread_create start_routine）。
// signal/raise 在 denylist，tier-0 未建 thunk → 预期报错。
use std::sync::atomic::{AtomicBool, Ordering};

static HIT: AtomicBool = AtomicBool::new(false);

extern "C" fn handler(_sig: i32) {
    HIT.store(true, Ordering::SeqCst);
}

fn main() {
    unsafe {
        libc::signal(libc::SIGUSR1, handler as usize);
        libc::raise(libc::SIGUSR1);
    }
    println!("handler hit = {}", HIT.load(Ordering::SeqCst));
}
