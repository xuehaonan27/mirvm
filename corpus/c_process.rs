#!/usr/bin/env mirvm
---
[dependencies]
---
// Subprocesses via std::process::Command: on Unix std uses posix_spawn when it can (not
// on the foreign denylist) and fork+exec otherwise, so this pins the passthrough down.
use std::process::Command;

fn main() {
    let out = Command::new("echo").arg("hello from subprocess").output().unwrap();
    println!("echo status ok = {}", out.status.success());
    print!("echo stdout = {}", String::from_utf8_lossy(&out.stdout));

    let t = Command::new("true").status().unwrap();
    let f = Command::new("false").status().unwrap();
    println!("true code = {:?}, false code = {:?}", t.code(), f.code());
}
