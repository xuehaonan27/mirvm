// scoped threads + 线程 panic → join Err 恢复 + 线程内 TLS Drop
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
    // scoped：借用栈上数据
    let mut data = vec![1u32, 2, 3, 4];
    thread::scope(|s| {
        let (a, b) = data.split_at_mut(2);
        s.spawn(|| a.iter_mut().for_each(|x| *x *= 10));
        s.spawn(|| b.iter_mut().for_each(|x| *x *= 100));
    });
    println!("scoped = {data:?}");

    // 线程 panic → join Err，进程不倒
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

    // 线程内 TLS Drop（线程退出时运行析构）
    let t = thread::spawn(|| {
        GUARD.with(|_| {});
        println!("worker done");
    });
    t.join().unwrap();
    println!("all done");
}
