#!/usr/bin/env mirvm
---
[dependencies]
---
// 子进程：std::process::Command。std 在 Unix 上按情况走 posix_spawn（不在 denylist）
// 或 fork+exec（denylist）。压"子进程 = 真 OS 进程 passthrough"的处置。
use std::process::Command;

fn main() {
    let out = Command::new("echo").arg("hello from subprocess").output().unwrap();
    println!("echo status ok = {}", out.status.success());
    print!("echo stdout = {}", String::from_utf8_lossy(&out.stdout));

    let t = Command::new("true").status().unwrap();
    let f = Command::new("false").status().unwrap();
    println!("true code = {:?}, false code = {:?}", t.code(), f.code());
}
