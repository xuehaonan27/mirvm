#!/usr/bin/env mirvm
---
[dependencies]
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "time", "sync"] }
---
// Multi-threaded runtime: the worker threads each block on a real epoll_wait. Expected:
// the cooperative scheduler multiplexes them onto one thread, so the workers deadlock.
fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let sum = rt.block_on(async {
        let mut handles = vec![];
        for i in 0..8u64 {
            handles.push(tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                i * i
            }));
        }
        let mut s = 0u64;
        for h in handles { s += h.await.unwrap(); }
        s
    });
    println!("sum of squares = {sum}");
}
