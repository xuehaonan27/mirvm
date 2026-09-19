// A future that actually suspends: first poll returns Pending, second returns Ready.
// Drives the async fn through a real save-state→Pending→resume cycle, with poll count > 1.
use std::future::Future;
use std::pin::{pin, Pin};
use std::task::{Context, Poll, Waker};

struct PendOnce { polled: bool, val: u32 }
impl Future for PendOnce {
    type Output = u32;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        if self.polled { Poll::Ready(self.val) }
        else { self.polled = true; cx.waker().wake_by_ref(); Poll::Pending }
    }
}
fn pend_once(v: u32) -> PendOnce { PendOnce { polled: false, val: v } }

async fn outer() -> u32 {
    let a = pend_once(10).await;   // suspends once
    let b = pend_once(a + 5).await; // suspends once
    a + b
}

fn main() {
    let mut fut = pin!(outer());
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut polls = 0;
    let result = loop {
        polls += 1;
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => break v,
            Poll::Pending => continue,
        }
    };
    println!("result = {result}, polls = {polls}");  // a=10, b=15 → 25; polls 3 times
}
