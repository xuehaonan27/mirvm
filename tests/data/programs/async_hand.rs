// Manually poll an async state machine: no tokio needed, just core/std.
// Purpose: prove mirvm treats an async future as an ordinary MIR state machine with zero special support.
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

async fn inner(x: u32) -> u32 { x + 1 }
async fn outer() -> u32 {
    let a = inner(10).await;   // await point 1
    let b = inner(a).await;    // await point 2
    a + b
}

fn main() {
    let mut fut = pin!(outer());
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    // Manually drive the state machine: poll repeatedly until Ready
    let mut polls = 0;
    let result = loop {
        polls += 1;
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => break v,
            Poll::Pending => continue, // noop waker: immediately poll again
        }
    };
    println!("result = {result}, polls = {polls}");
}
