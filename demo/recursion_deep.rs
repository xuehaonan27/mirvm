// Permanent differential probe for stack-depth fidelity: recursion depths that a native 8 MiB
// main stack carries easily must also work under mirvm. Three shapes: deep recursion on the
// main thread / explicit 2 MiB small-stack thread / default stack thread -- covering both the
// main large stack and pthread_create attr enlargement, with ample native headroom (~48B/frame).

fn count(n: u64) -> u64 {
    if n == 0 { 0 } else { 1 + count(n - 1) }
}

// Recursion with a larger local (raises the per-frame byte cost, exercising the operand area)
fn chunky(n: u64) -> u64 {
    let buf = [n; 16];
    if n == 0 { buf[15] } else { buf[0] % 7 + chunky(n - 1) }
}

fn main() {
    println!("main 50k = {}", count(50_000));
    println!("main chunky 20k = {}", chunky(20_000));

    let small = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(|| count(8_000))
        .unwrap();
    println!("thread 2MiB 8k = {}", small.join().unwrap());

    let default = std::thread::spawn(|| count(20_000));
    println!("thread default 20k = {}", default.join().unwrap());
}
