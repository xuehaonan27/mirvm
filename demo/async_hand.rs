// 手动 poll 一个 async 状态机：不需要 tokio，只用 core/std。
// 目的：证明 mirvm 把 async future 当普通 MIR 状态机解释，零特殊支持。
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

async fn inner(x: u32) -> u32 { x + 1 }
async fn outer() -> u32 {
    let a = inner(10).await;   // await 点 1
    let b = inner(a).await;    // await 点 2
    a + b
}

fn main() {
    let mut fut = pin!(outer());
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    // 手动驱动状态机：反复 poll 到 Ready
    let mut polls = 0;
    let result = loop {
        polls += 1;
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => break v,
            Poll::Pending => continue, // noop waker：立即再 poll
        }
    };
    println!("result = {result}, polls = {polls}");
}
