//! T1-c unwind 产品化探针（m5.4-design §3.4）：
//! try_call 着陆（Cleanup 边 panic → cleanup pad 在 JIT 帧内执行 + Drop
//! 顺序与 native 一致）+ catch_unwind 捕获 + payload 保全 + Resume 续传。
//! 三维（native / JIT-off / JIT=1）逐字节一致 + MIRVM_JIT_DEBUG 发布实证。
struct D(u64);
impl Drop for D {
    fn drop(&mut self) {
        println!("drop-{}", self.0);
    }
}

#[inline(never)]
fn bomb() -> u64 {
    panic!("bomb-42");
}

#[inline(never)]
fn guarded() -> u64 {
    std::panic::catch_unwind(|| bomb() + 1)
        .unwrap_err()
        .downcast_ref::<&'static str>()
        .map(|s| s.len() as u64)
        .unwrap_or(0)
}

#[inline(never)]
fn nested() -> u64 {
    let _d1 = D(1);
    let _d2 = D(2);
    guarded()
}

fn main() {
    let mut r = 0;
    for _ in 0..30000 {
        r = nested();
    }
    println!("r={r}");
    let _d = D(9);
}
