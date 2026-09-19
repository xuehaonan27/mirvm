// atexit LIFO callbacks: the Rust drop-on-exit idiom across the C boundary.
use std::os::raw::c_int;
use std::sync::atomic::{AtomicU32, Ordering};

static ORDER: AtomicU32 = AtomicU32::new(0);
unsafe extern "C" { fn atexit(f: extern "C" fn()) -> c_int; }

extern "C" fn first() { println!("exit[{}] first", ORDER.fetch_add(1, Ordering::SeqCst)); }
extern "C" fn second() { println!("exit[{}] second", ORDER.fetch_add(1, Ordering::SeqCst)); }
extern "C" fn third() { println!("exit[{}] third", ORDER.fetch_add(1, Ordering::SeqCst)); }

fn main() {
    unsafe { atexit(first); atexit(second); atexit(third); }
    println!("main returning; atexit runs LIFO");
}
