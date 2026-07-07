#!/usr/bin/env mirvm
---
[dependencies]
crossbeam = "0.8"
---
// crossbeam：scoped 线程 + MPMC 通道。压测真实线程 + 同步原语。
use crossbeam::channel;
use crossbeam::thread;

fn main() {
    let (tx, rx) = channel::unbounded();

    thread::scope(|s| {
        // 4 个生产者
        for p in 0..4 {
            let tx = tx.clone();
            s.spawn(move |_| {
                for i in 0..25 {
                    tx.send(p * 100 + i).unwrap();
                }
            });
        }
        drop(tx);

        // 主线程消费
        let mut sum: i64 = 0;
        let mut n = 0;
        for v in rx.iter() {
            sum += v as i64;
            n += 1;
        }
        println!("received {n} messages, sum = {sum}");
    })
    .unwrap();
}
