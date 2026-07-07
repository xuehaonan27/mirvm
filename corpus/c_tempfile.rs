#!/usr/bin/env mirvm
---
[dependencies]
tempfile = "3"
---
// 真文件 IO：创建临时文件、写、元数据、重开读回、drop 删除。
// 压 open/write/fstat/read/unlink 直通深度。
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
