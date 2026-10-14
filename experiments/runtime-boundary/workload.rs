#[inline(never)]
fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("fib") => println!("{}", fib(32)),
        Some("hold") => {
            println!("ready");
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        _ => println!("ready"),
    }
}
