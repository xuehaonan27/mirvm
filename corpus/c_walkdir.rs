#!/usr/bin/env mirvm
---
[dependencies]
tempfile = "3"
walkdir = "2"
---
// 目录遍历：建临时目录树，递归 walk，收集文件名。
// 压 mkdir/opendir/readdir(getdents)/lstat 直通。
use std::fs;
use walkdir::WalkDir;

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), "a").unwrap();
    fs::write(root.join("sub/b.txt"), "b").unwrap();
    fs::write(root.join("sub/c.txt"), "c").unwrap();

    let mut names: Vec<String> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    println!("files: {names:?}");

    let dirs = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .count();
    println!("dir entries (incl root): {dirs}");
}
