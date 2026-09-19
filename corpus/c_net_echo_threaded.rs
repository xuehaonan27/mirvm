#!/usr/bin/env mirvm
---
[dependencies]
---
// Network hazard probe (expected to hang; excluded from the automated batch): the client's
// read_exact waits for the server thread's echo, which never runs under cooperative scheduling.
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
    client.read_exact(&mut resp).unwrap(); // blocks on the server thread -> hang
    println!("echo = {:?}", std::str::from_utf8(&resp).unwrap());
    server.join().unwrap();
}
