// M5.2 D8a 栈深度保真的永久差分探针：native 8 MiB 主栈轻松承载的递归深度，
// mirvm 必须同样承载（旧 8000 帧硬上限对 native 栈界严重失真）。
// 三形态：主线程深递归 / 显式 2 MiB 小栈线程 / 默认栈线程——覆盖主执行大栈、
// pthread_create attr 放大两条路。深度选择让 native 侧也有充分余量（帧 ~48B）。

fn count(n: u64) -> u64 {
    if n == 0 { 0 } else { 1 + count(n - 1) }
}

// 带较大局部的递归（放大每帧字节成本，探操作数区）
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
