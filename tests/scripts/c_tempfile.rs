#!/usr/bin/env mirvm
---
[dependencies]
tempfile = "3"
---
// Real file IO: create a temporary file, write, stat, reopen and read back, delete on drop.
// Exercises the passthrough depth of open/write/fstat/read/unlink.
use std::io::{Read, Write};

fn main() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    write!(f, "line1\nline2\n").unwrap();
    f.flush().unwrap();
    let path = f.path().to_path_buf();

    let meta = std::fs::metadata(&path).unwrap();
    println!("size = {}", meta.len());

    let mut f2 = f.reopen().unwrap();
    let mut s = String::new();
    f2.read_to_string(&mut s).unwrap();
    println!("content = {s:?}");

    println!("exists before drop = {}", path.exists());
    drop(f);
    println!("exists after drop  = {}", path.exists());
}
