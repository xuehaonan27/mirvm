#!/usr/bin/env mirvm
---
[dependencies]
crossbeam = "0.8"
---
// crossbeam: scoped threads + MPMC channel. Exercises real threads and sync primitives.
use crossbeam::channel;
use crossbeam::thread;

fn main() {
    let (tx, rx) = channel::unbounded();

    thread::scope(|s| {
        // 4 producers
        for p in 0..4 {
            let tx = tx.clone();
            s.spawn(move |_| {
                for i in 0..25 {
                    tx.send(p * 100 + i).unwrap();
                }
            });
        }
        drop(tx);

        // Main thread consumes
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
