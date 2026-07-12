#!/usr/bin/env mirvm
---
---

fn main() {
    // 在 guest frame/IP 映射层完成前，宁可明确拒绝也不能把
    // libffi/解释器的宿主栈伪装成 guest backtrace。
    println!("{:?}", std::backtrace::Backtrace::force_capture());
}
