#!/usr/bin/env mirvm
---
[dependencies]
---
// 单线程 loopback TCP：write-before-read 排序，每个阻塞调用的数据/连接
// 在调用前已就绪（内核 loopback 缓冲 + accept backlog 自释放），不依赖另一 guest 线程。
// 压 socket/bind/listen/connect/accept/send/recv/close 全套（std::net → libc 直通）。
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    println!("bound loopback = {}", addr.ip().is_loopback());

    // connect：loopback + listener 在，内核完成握手进 backlog，无需 accept 先行
    let mut client = TcpStream::connect(addr).unwrap();
    client.write_all(b"ping").unwrap(); // 缓冲

    // backlog 里已有连接，accept 立即返回
    let (mut server, peer) = listener.accept().unwrap();
    println!("peer loopback = {}", peer.ip().is_loopback());

    // "ping" 已在 server 接收缓冲，read 不阻塞
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).unwrap();
    println!("server got = {:?}", std::str::from_utf8(&buf).unwrap());

    // server 回，client 读（数据在读之前已缓冲）
    server.write_all(b"pong").unwrap();
    let mut resp = [0u8; 4];
    client.read_exact(&mut resp).unwrap();
    println!("client got = {:?}", std::str::from_utf8(&resp).unwrap());
}
