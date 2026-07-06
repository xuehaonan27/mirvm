// mpsc channel：生产者-消费者（求和结果与调度无关）
use std::sync::mpsc;
use std::thread;

fn main() {
    let (tx, rx) = mpsc::channel::<u64>();
    let producers: Vec<_> = (0..3)
        .map(|p| {
            let tx = tx.clone();
            thread::spawn(move || {
                for i in 0..100u64 {
                    tx.send(p * 1000 + i).unwrap();
                }
            })
        })
        .collect();
    drop(tx);

    let consumer = thread::spawn(move || {
        let mut sum = 0u64;
        let mut count = 0;
        while let Ok(v) = rx.recv() {
            sum += v;
            count += 1;
        }
        (sum, count)
    });

    for p in producers {
        p.join().unwrap();
    }
    let (sum, count) = consumer.join().unwrap();
    println!("received {count} messages, sum = {sum}");
}
