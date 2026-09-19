#!/usr/bin/env mirvm
---
[dependencies]
libc = "0.2"
---
// L2 fork-generation contract probe (design mirvm_high_performance_log.md §6.3),
// extended by L3 to cover a second guest thread.
//
// A capture session is armed before the guest starts. The parent records two
// instrumented syscalls, a spawned guest thread records three more, the parent
// forks through the libc wrapper, and the child records two of its own before
// exiting normally. Correct behaviour:
//
//   events-<parent pid>-0.mlog   parent thread + guest thread records, generation 0
//   events-<child pid>-1.mlog    child records, generation 1
//
// The child's file only has content when the fork child builds its own session,
// attaches its own producer and seals its page at exit; before L2 all three were
// missing and the child's event stream was silently empty. The guest thread is
// what L3 needs: each pthread gets its own activation, its own producer and, in
// the trace domain, its own value in the pinned register.
//
// `libc::syscall` is the only form registered as the recording builtin
// (lower/builtins.rs), so the probe must use it to produce events at all.

fn main() {
    let first = unsafe { libc::syscall(libc::SYS_getpid as libc::c_long) };
    let second = unsafe { libc::syscall(libc::SYS_getpid as libc::c_long) };
    println!("parent-pid={first} same={}", first == second);

    let worker = std::thread::spawn(|| {
        let mut seen = 0i64;
        for _ in 0..3 {
            seen += unsafe { libc::syscall(libc::SYS_getpid as libc::c_long) };
        }
        seen > 0
    });
    if !worker.join().unwrap_or(false) {
        std::process::exit(4);
    }

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let c1 = unsafe { libc::syscall(libc::SYS_getpid as libc::c_long) };
        let c2 = unsafe { libc::syscall(libc::SYS_getpid as libc::c_long) };
        // Normal exit so the lingering-session hook seals the child's page and
        // publishes its file with a clean SessionEnd.
        if c1 == c2 {
            std::process::exit(0);
        }
        std::process::exit(3);
    }
    let mut status = 0i32;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    println!("child-exit={}", (status >> 8) & 0xff);
}
