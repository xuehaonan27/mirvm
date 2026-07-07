#!/usr/bin/env mirvm
---
[dependencies]
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "time", "sync"] }
---
// 多线程运行时：worker 线程会各自阻塞在真 epoll_wait 上。
// 预期：协作调度器把它们多路复用到一条真线程 → worker 互等 → 死锁/挂起。
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
