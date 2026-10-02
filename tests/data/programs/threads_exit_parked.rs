// A worker parked forever and a main that returns anyway. Native `exit` does not join the process's
// threads, so the process ends with the worker still parked in the guest; the Engine must not add a
// wait native does not have.
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

fn main() {
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let worker = {
        let pair = Arc::clone(&pair);
        thread::spawn(move || {
            let (lock, cvar) = &*pair;
            let mut go = lock.lock().unwrap();
            while !*go {
                go = cvar.wait(go).unwrap();
            }
            println!("unreachable: nothing ever notifies the worker");
        })
    };
    // Detach, never join: joining would be this guest asking to wait for the worker.
    drop(worker);
    println!("main returns with a worker parked");
}
