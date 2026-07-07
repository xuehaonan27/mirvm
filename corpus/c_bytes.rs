#!/usr/bin/env mirvm
---
[dependencies]
bytes = "1"
---
// 引用计数字节缓冲：Arc 底层 + 共享切片 + unsafe 指针算术。压原子 refcount + 别名。
use bytes::{Buf, BufMut, BytesMut};

fn main() {
    let mut buf = BytesMut::with_capacity(64);
    buf.put_u32(0xDEADBEEF);
    buf.put(&b"hello"[..]);

    let bytes = buf.freeze(); // → Bytes（Arc 支撑）
    let shared = bytes.clone(); // 共享同一底层（原子 refcount++）
    let mut tail = bytes.slice(4..9); // 零拷贝切片，别名同一 buffer

    println!("total len = {}", shared.len());
    println!("tail = {:?}", std::str::from_utf8(&tail[..]).unwrap());
    println!("first byte of tail = {:#x}", tail.get_u8());

    // 前 4 字节是大端 u32
    let mut head = shared.slice(0..4);
    println!("head u32 = {:#x}", head.get_u32());
}
