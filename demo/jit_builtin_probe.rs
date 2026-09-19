//! CallBuiltin JIT probe:
//! allocation fast paths (Vec push growth / Box::new large array / String concatenation /
//! vec! zeroing -> RustAlloc/RustRealloc/RustDealloc/RustAllocZeroed, all entering the same
//! mirvm_alloc engine heap entry) + the CatchUnwind arm (catch_unwind catches a panic and
//! downcasts the payload) + the HostWrite arm (libc write straight through).
//! Must be byte-for-byte identical across native / JIT-off / JIT=1, with MIRVM_JIT_DEBUG
//! proving publication. The loops give the asynchronous compile thread time to run.

use std::panic;

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

#[inline(never)]
fn vec_grow(n: u64) -> u64 {
    let mut v: Vec<u64> = Vec::new();
    for i in 0..n {
        v.push(i);
    }
    v.iter().sum()
}

#[inline(never)]
fn box_big(i: usize) -> u64 {
    let b = Box::new([7u64; 256]);
    b[i % 256]
}

#[inline(never)]
fn zeroed_block(n: usize) -> u64 {
    let z = vec![0u64; n];
    z.iter().sum::<u64>() + z.len() as u64
}

#[inline(never)]
fn string_build(n: u64) -> usize {
    let mut s = String::new();
    for i in 0..n {
        s.push_str("ab");
        s.push_str(&i.to_string());
    }
    s.len()
}

#[inline(never)]
fn catch_payload(round: u64) -> String {
    let r = panic::catch_unwind(|| -> u64 {
        if round % 2 == 0 {
            panic!("boom-{round}");
        }
        round * 3
    });
    match r {
        Ok(v) => format!("ok-{v}"),
        Err(e) => {
            let m = e.downcast_ref::<String>().map(String::as_str).unwrap_or("?");
            format!("caught-{m}")
        }
    }
}

#[inline(never)]
fn raw_write(msg: &str) -> isize {
    unsafe { write(1, msg.as_ptr(), msg.len()) }
}

fn main() {
    // Silent panic hook: the catch path only inspects the payload, avoiding 30k lines of hook
    // output that would slow the comparison (catch.rs already covers the default hook's stderr).
    panic::set_hook(Box::new(|_| {}));
    let mut acc = 0u64;
    for i in 0..30000u64 {
        acc ^= vec_grow(64);
        acc = acc.wrapping_add(box_big(i as usize));
        acc ^= zeroed_block(128);
        if i % 1000 == 0 {
            acc = acc.wrapping_add(string_build(20) as u64);
        }
        acc = acc.wrapping_add(catch_payload(i).len() as u64);
    }
    println!("alloc/catch acc={acc}");
    let n = raw_write("raw-write-ok\n");
    println!("write ret={n}");
    let p = catch_payload(2);
    println!("payload: {p}");
}
