#!/usr/bin/env mirvm
---
---
// guest single-threaded fork + child work + waitpid reap. The pid is never
// printed because it is not deterministic; the child exit code is.
unsafe extern "C" {
    #[link_name = "fork"]
    fn c_fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, opts: i32) -> i32;
    #[link_name = "_exit"]
    fn c_exit(code: i32) -> !;
}

fn main() {
    let mut oks = 0;
    for k in 1..=3 {
        let pid = unsafe { c_fork() };
        if pid == 0 {
            // child: heap + compute; the exit code carries the result back
            let v: Vec<i32> = (0..100).map(|i| i * k).collect();
            let s: i32 = v.iter().sum();
            unsafe { c_exit((s % 128) as i32) };
        }
        let mut status = 0i32;
        unsafe { waitpid(pid, &mut status, 0) };
        let code = (status >> 8) & 0xff;
        // 4950*k % 128
        if code == (4950 * k) % 128 {
            oks += 1;
        }
    }
    println!("fork children ok = {oks}/3");
}
