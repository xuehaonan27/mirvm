#!/usr/bin/env mirvm
---
[dependencies]
libc = "0.2"
---
// Signal handling: register a guest handler, then raise. The handler is interpreted code
// with no machine address, while kernel signal delivery needs a real one, so a thunk is
// required (same as pthread_create's start_routine). The engine must report "guest handler
// unsupported" rather than fake success; once supported, the oracle requires handler=true.
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
