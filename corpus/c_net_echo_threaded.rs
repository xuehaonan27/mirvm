#!/usr/bin/env mirvm
---
[dependencies]
---
// §2.1 网络形态危险探针（预期挂死，不入自动批）：
// client.read_exact 阻塞等 server 线程 echo，但协作调度下 server 线程跑不起来 → 挂。
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).unwrap();
        s.write_all(&buf).unwrap(); // echo
    });

    let mut client = TcpStream::connect(addr).unwrap();
    client.write_all(b"echo").unwrap();
    let mut resp = [0u8; 4];
    client.read_exact(&mut resp).unwrap(); // 阻塞等 server 线程 → §2.1 挂死
    println!("echo = {:?}", std::str::from_utf8(&resp).unwrap());
    server.join().unwrap();
}
