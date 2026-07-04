// catch_unwind：捕获 panic 恢复执行 + Drop 在 unwind 中运行
use std::panic;

struct Guard(&'static str);
impl Drop for Guard {
    fn drop(&mut self) {
        println!("drop: {}", self.0);
    }
}

fn main() {
    let _outer = Guard("outer");
    let r = panic::catch_unwind(|| {
        let _inner = Guard("inner");
        println!("before panic");
        panic!("boom-{}", 42);
    });
    match r {
        Ok(()) => println!("no panic??"),
        Err(e) => {
            let msg = e.downcast_ref::<String>().map(String::as_str).unwrap_or("?");
            println!("caught: {msg}");
        }
    }
    println!("recovered, keep going");
}
