//! T1-c unwind productization probe (m5.4-design §3.4):
//! try_call landing (Cleanup-edge panic → cleanup pad executes inside JIT frame + Drop
//! order matches native) + catch_unwind capture + payload preservation + Resume propagation.
//! Byte-identical across three dimensions (native / JIT-off / JIT=1) + MIRVM_JIT_DEBUG release evidence.
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
