#!/usr/bin/env mirvm
---
[dependencies]
bytes = "1"
---
// Refcounted byte buffer: Arc backing, shared slices, unsafe pointer arithmetic; atomic refcount + aliasing.
use bytes::{Buf, BufMut, BytesMut};

fn main() {
    let mut buf = BytesMut::with_capacity(64);
    buf.put_u32(0xDEADBEEF);
    buf.put(&b"hello"[..]);

    let bytes = buf.freeze(); // -> Bytes (Arc-backed)
    let shared = bytes.clone(); // shares the same backing (atomic refcount++)
    let mut tail = bytes.slice(4..9); // zero-copy slice aliasing the same buffer

    println!("total len = {}", shared.len());
    println!("tail = {:?}", std::str::from_utf8(&tail[..]).unwrap());
    println!("first byte of tail = {:#x}", tail.get_u8());

    // The first 4 bytes are a big-endian u32
    let mut head = shared.slice(0..4);
    println!("head u32 = {:#x}", head.get_u32());
}
