// scoped threads + thread panic → join Err recovery + in-thread TLS Drop
use std::thread;

thread_local! {
    static GUARD: Guard = Guard;
}

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        println!("tls dtor ran");
    }
}

fn main() {
    // scoped: borrow stack data
    let mut data = vec![1u32, 2, 3, 4];
    thread::scope(|s| {
        let (a, b) = data.split_at_mut(2);
        s.spawn(|| a.iter_mut().for_each(|x| *x *= 10));
        s.spawn(|| b.iter_mut().for_each(|x| *x *= 100));
    });
    println!("scoped = {data:?}");

    // thread panic → join Err, process survives
    let h = thread::spawn(|| {
        panic!("worker exploded");
    });
    match h.join() {
        Ok(()) => println!("no panic??"),
        Err(e) => {
            let msg = e.downcast_ref::<&str>().copied().unwrap_or("?");
            println!("joined Err: {msg}");
        }
    }
    println!("main survives");

    // in-thread TLS Drop (destructors run on thread exit)
    let t = thread::spawn(|| {
        GUARD.with(|_| {});
        println!("worker done");
    });
    t.join().unwrap();
    println!("all done");
}
