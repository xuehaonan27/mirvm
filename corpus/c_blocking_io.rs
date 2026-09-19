#!/usr/bin/env mirvm
---
[dependencies]
---
// Guest thread A blocks in read() on a real socket (real syscall passthrough), guest thread
// B writes. Real threads are fine; cooperative scheduling blocks the whole thread -> hang.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::thread;

fn main() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    let h = thread::spawn(move || {
        thread::sleep(std::time::Duration::from_millis(50));
        b.write_all(b"hi").unwrap();
    });
    let mut buf = [0u8; 2];
    a.read_exact(&mut buf).unwrap(); // truly blocking read()
    println!("got: {:?}", &buf);
    h.join().unwrap();
}
