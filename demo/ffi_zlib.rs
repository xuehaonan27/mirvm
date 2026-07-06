#!/usr/bin/env mirvm
---
[dependencies]
libz-sys = "1"
---
use std::os::raw::c_ulong;
fn main() {
    let input = b"mirvm calls zlib through libffi \xe4\xb8\xad\xe6\x96\x87".repeat(20);
    let mut comp = vec![0u8; 4096];
    let mut clen: c_ulong = comp.len() as c_ulong;
    let rc = unsafe { libz_sys::compress(comp.as_mut_ptr(), &mut clen, input.as_ptr(), input.len() as c_ulong) };
    println!("compress rc={rc} {} -> {}", input.len(), clen);
    let mut deco = vec![0u8; input.len()];
    let mut dlen: c_ulong = deco.len() as c_ulong;
    let rc2 = unsafe { libz_sys::uncompress(deco.as_mut_ptr(), &mut dlen, comp.as_ptr(), clen) };
    deco.truncate(dlen as usize);
    println!("uncompress rc={rc2} roundtrip_ok={}", deco == input);
    let c = unsafe { libz_sys::crc32(0, input.as_ptr(), input.len() as u32) };
    println!("crc32 = {c:#010x}");
}
