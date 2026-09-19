// Mutex counter + Arc sharing + Condvar notification
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

fn main() {
    // Mutex contention counter
    let counter = Arc::new(Mutex::new(0u64));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let c = Arc::clone(&counter);
            thread::spawn(move || {
                for _ in 0..500 {
                    *c.lock().unwrap() += 1;
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    println!("counter = {}", *counter.lock().unwrap());

    // Condvar: wait for ready signal
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let p2 = Arc::clone(&pair);
    let waiter = thread::spawn(move || {
        let (lock, cvar) = &*p2;
        let mut ready = lock.lock().unwrap();
        while !*ready {
            ready = cvar.wait(ready).unwrap();
        }
        "woken"
    });
    {
        let (lock, cvar) = &*pair;
        *lock.lock().unwrap() = true;
        cvar.notify_one();
    }
    println!("condvar: {}", waiter.join().unwrap());
}
