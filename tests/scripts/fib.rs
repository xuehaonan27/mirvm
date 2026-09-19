// mirvm's first test input: pure computation + std collections + formatted output.
// M0: only used to observe MIR with --dump-mir; from M1 onward the interpreted result must match native compilation.

fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    let xs: Vec<u64> = (0..10).map(fib).collect();
    let sum: u64 = xs.iter().sum();
    println!("fib(0..10) = {xs:?}, sum = {sum}");
}
