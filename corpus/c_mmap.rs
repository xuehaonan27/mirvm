#!/usr/bin/env mirvm
---
[dependencies]
memmap2 = "0.9"
---
// mmap：file-backed + 匿名映射。压 mmap/munmap 直通 + guest 对内核映射区的访问。
// 用 std::fs（libc，非 rustix）建文件，避开 rustix 的 asm 问题，隔离 mmap 本身。
use memmap2::MmapMut;
use std::fs::OpenOptions;

fn main() {
    // file-backed mmap
    let path = "/tmp/mirvm_mmap_test.bin";
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.set_len(4096).unwrap();

    let mut mmap = unsafe { MmapMut::map_mut(&file).unwrap() };
    mmap[0..5].copy_from_slice(b"hello"); // 写内核映射区
    mmap.flush().unwrap();
    println!("file mmap = {:?}", std::str::from_utf8(&mmap[0..5]).unwrap());
    drop(mmap);
    std::fs::remove_file(path).ok();

    // 匿名映射
    let mut anon = MmapMut::map_anon(8192).unwrap();
    anon[100] = 42;
    println!("anon[100] = {}, len = {}", anon[100], anon.len());
}
