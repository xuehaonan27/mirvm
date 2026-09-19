// spawn + join: return-value passing, multi-thread concurrent compute (output independent of scheduling order)
use std::thread;

fn main() {
    let handles: Vec<_> = (0..4)
        .map(|i| thread::spawn(move || (0..1000u64).map(|x| x * (i + 1)).sum::<u64>()))
        .collect();
    let results: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    println!("results = {results:?}");
    println!("total = {}", results.iter().sum::<u64>());

    // nested spawn
    let outer = thread::spawn(|| {
        let inner = thread::spawn(|| 21u32);
        inner.join().unwrap() * 2
    });
    println!("nested = {}", outer.join().unwrap());
}
