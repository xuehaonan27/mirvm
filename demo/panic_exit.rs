// Uncaught panic: should print a message and exit with code 101
fn main() {
    let v = vec![1, 2, 3];
    println!("sum={}", v.iter().sum::<i32>());
    let idx = 7;
    let _ = v[idx]; // out of bounds → panic_bounds_check
    println!("unreachable");
}
