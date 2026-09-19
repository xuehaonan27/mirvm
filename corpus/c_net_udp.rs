#!/usr/bin/env mirvm
---
[dependencies]
---
// Single-threaded loopback UDP: send before recv, so the datagram is already queued and recv
// does not block. Exercises socket(SOCK_DGRAM)/bind/sendto/recvfrom.
use std::net::UdpSocket;

fn main() {
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let a_addr = a.local_addr().unwrap();
    let b_addr = b.local_addr().unwrap();

    b.send_to(b"hello udp", a_addr).unwrap(); // send first so the datagram queues on a

    let mut buf = [0u8; 32];
    let (n, from) = a.recv_from(&mut buf).unwrap();
    println!("udp got {} bytes = {:?}", n, std::str::from_utf8(&buf[..n]).unwrap());
    println!("from b = {}", from == b_addr);
}
