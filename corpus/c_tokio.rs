#!/usr/bin/env mirvm
---
[dependencies]
tokio = { version = "1", features = ["rt", "macros", "time", "sync"] }
---
// Async runtime: current_thread scheduler + timers + tasks. Exercises the os:: boundary
// (the reactor starts epoll/eventfd) and async execution.
use tokio::sync::mpsc;

async fn worker(id: u32, tx: mpsc::Sender<u32>) {
    for i in 0..5 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        tx.send(id * 100 + i).await.unwrap();
    }
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let total = rt.block_on(async {
        let (tx, mut rx) = mpsc::channel(32);
        for id in 0..3u32 {
            tokio::spawn(worker(id, tx.clone()));
        }
        drop(tx);
        let mut sum = 0u64;
        let mut n = 0;
        while let Some(v) = rx.recv().await {
            sum += v as u64;
            n += 1;
        }
        (n, sum)
    });

    println!("received {} msgs, sum = {}", total.0, total.1);
}
