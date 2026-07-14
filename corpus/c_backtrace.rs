#!/usr/bin/env mirvm
---
---
// M5.2 D8e：guest backtrace 影子帧。backtrace 的**精确文本**非 well-defined 可观察
// （地址随运行变、符号名依赖 debuginfo；ram-spec §2），故 oracle 用不变式：
// ① 捕获成功（status=Captured）② 非空 ③ **深度被如实反映**（更深的调用点 → 更多帧）。
// 这些不变式 native 与 mirvm 都满足，两侧打印同一确定行。mirvm 的合成 IP 不经
// dladdr 符号化（诚实 <unknown>，禁止伪造宿主符号）——故不逐字节对拍文本。
#![feature(backtrace_frames)]
use std::backtrace::{Backtrace, BacktraceStatus};
use std::hint::black_box;

// black_box 夹在递归调用两侧，阻止尾调用优化把递归压成循环（否则 native 帧数不随
// n 增长，深度不变式失效）。
fn deep(n: u32) -> usize {
    if n == 0 {
        black_box(Backtrace::force_capture().frames().len())
    } else {
        black_box(deep(black_box(n) - 1))
    }
}

fn main() {
    let bt = Backtrace::force_capture();
    assert!(
        matches!(bt.status(), BacktraceStatus::Captured),
        "backtrace 未捕获"
    );
    let shallow = deep(0);
    let deeper = deep(30);
    assert!(shallow >= 1, "捕获到 0 帧");
    assert!(
        deeper >= shallow + 30,
        "递归深度未反映到影子帧：{shallow} -> {deeper}"
    );
    println!("backtrace: captured, non-empty, depth reflected (+30 frames)");
}
