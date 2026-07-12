#!/usr/bin/env mirvm
---
[dependencies]
libc = "0.2"
---
// 信号处理：注册 guest 处理函数 + raise。handler 是解释代码（无机器地址），
// 内核信号投递需要真机器地址 → 需 thunk（同 pthread_create start_routine）。
// 当前引擎须明确报“不支持 guest handler”，不可伪造成功；未来真实支持后，本 fixture
// 仍要求 handler 被调用并以 handler=true 作为 oracle。
use std::sync::atomic::{AtomicBool, Ordering};

static HIT: AtomicBool = AtomicBool::new(false);

extern "C" fn handler(_sig: i32) {
    HIT.store(true, Ordering::SeqCst);
}

fn main() {
    unsafe {
        libc::signal(libc::SIGUSR1, handler as *const () as usize);
        libc::raise(libc::SIGUSR1);
    }
    let hit = HIT.load(Ordering::SeqCst);
    println!("handler hit = {hit}");
    assert!(hit, "SIGUSR1 handler was not invoked");
}
