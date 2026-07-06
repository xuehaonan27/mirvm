// 一个真正会挂起的 future：第一次 poll 返回 Pending，第二次返回 Ready。
// 驱动 async fn 经历真实的"存状态→Pending→恢复"循环，poll 次数 > 1。
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
    let a = pend_once(10).await;   // 挂起 1 次
    let b = pend_once(a + 5).await; // 挂起 1 次
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
    println!("result = {result}, polls = {polls}");  // a=10,b=15 → 25；poll 3 次
}
