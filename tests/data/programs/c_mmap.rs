#!/usr/bin/env mirvm
---
[dependencies]
memmap2 = "0.9"
---
// mmap: file-backed and anonymous mappings. Exercises mmap/munmap passthrough plus guest
// access to a kernel mapping. std::fs (libc, not rustix) creates the file, isolating mmap.
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
    mmap[0..5].copy_from_slice(b"hello"); // write into the kernel mapping
    mmap.flush().unwrap();
    println!("file mmap = {:?}", std::str::from_utf8(&mmap[0..5]).unwrap());
    drop(mmap);
    std::fs::remove_file(path).ok();

    // anonymous mapping
    let mut anon = MmapMut::map_anon(8192).unwrap();
    anon[100] = 42;
    println!("anon[100] = {}, len = {}", anon[100], anon.len());
}
