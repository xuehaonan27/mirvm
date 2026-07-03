// mirvm 的第一个测试输入：纯计算 + std 集合 + 格式化输出。
// M0: 只用于 --dump-mir 观察 MIR；M1 起要求解释执行结果与原生编译一致。

fn fib(n: u64) -> u64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    let xs: Vec<u64> = (0..10).map(fib).collect();
    let sum: u64 = xs.iter().sum();
    println!("fib(0..10) = {xs:?}, sum = {sum}");
}
