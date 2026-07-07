#!/usr/bin/env mirvm
---
[dependencies]
---
// §2.1 纯净版：guest 线程 A 在真 socket 上 read() 阻塞（真 syscall 直通），
// guest 线程 B 负责写。真线程下正常；协作调度下 A 阻塞整条真线程 → B 跑不起来 → 挂死。
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
    a.read_exact(&mut buf).unwrap(); // 真阻塞 read()
    println!("got: {:?}", &buf);
    h.join().unwrap();
}
