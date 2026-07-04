// 未捕获 panic：应打印消息并以退出码 101 结束
fn main() {
    let v = vec![1, 2, 3];
    println!("sum={}", v.iter().sum::<i32>());
    let idx = 7;
    let _ = v[idx]; // 越界 → panic_bounds_check
    println!("unreachable");
}
