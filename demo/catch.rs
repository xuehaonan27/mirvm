// catch_unwind: capture panic and resume execution + Drop runs during unwind
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
