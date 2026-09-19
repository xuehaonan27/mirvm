use std::time::{Duration, Instant};
use std::thread;
fn main() {
    let t0 = Instant::now();
    let h = thread::spawn(|| { thread::sleep(Duration::from_millis(50)); "slept" });
    println!("{}", h.join().unwrap());
    println!("elapsed >= 50ms: {}", t0.elapsed() >= Duration::from_millis(50));
    let (tx, rx) = std::sync::mpsc::channel::<u8>();
    let r = rx.recv_timeout(Duration::from_millis(30));
    println!("timeout: {:?}", r.is_err());
    drop(tx);
}
