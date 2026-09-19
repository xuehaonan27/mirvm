#!/usr/bin/env mirvm
---
[dependencies]
---
// Single-threaded loopback TCP over std::net -> libc: writes precede reads, so every
// blocking call's data/connection is ready up front (kernel loopback buffer + accept
// backlog), no second guest thread. Exercises socket/bind/listen/connect/accept/send/recv/close.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    println!("bound loopback = {}", addr.ip().is_loopback());

    // connect: loopback + listener live; the kernel handshakes into the backlog before accept
    let mut client = TcpStream::connect(addr).unwrap();
    client.write_all(b"ping").unwrap(); // buffered

    // a connection is already in the backlog, so accept returns immediately
    let (mut server, peer) = listener.accept().unwrap();
    println!("peer loopback = {}", peer.ip().is_loopback());

    // "ping" is already in the server receive buffer, so read does not block
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).unwrap();
    println!("server got = {:?}", std::str::from_utf8(&buf).unwrap());

    // server replies, client reads (the data was buffered before the read)
    server.write_all(b"pong").unwrap();
    let mut resp = [0u8; 4];
    client.read_exact(&mut resp).unwrap();
    println!("client got = {:?}", std::str::from_utf8(&resp).unwrap());
}
