#!/usr/bin/env mirvm
---
---
// Guest backtrace shadow frames. A backtrace's **exact text** is not well-defined (addresses
// vary per run, symbol names depend on debuginfo), so the oracle uses invariants: (1) capture
// succeeds (status=Captured) (2) non-empty (3) **depth is reflected truthfully** (deeper call
// site -> more frames). native and mirvm both satisfy these and print the same deterministic
// line. Captured addresses must remain symbolizable after the capturing function returns.
#![feature(backtrace_frames)]
use std::backtrace::{Backtrace, BacktraceStatus};
use std::hint::black_box;

// black_box wraps the recursive call to stop tail-call optimization from collapsing the
// recursion into a loop (otherwise native frame count would not grow with n; depth breaks).
fn deep(n: u32) -> usize {
    if n == 0 {
        black_box(Backtrace::force_capture().frames().len())
    } else {
        black_box(deep(black_box(n) - 1))
    }
}

#[inline(never)]
fn named_frame_present() -> bool {
    format!("{:?}", Backtrace::force_capture()).contains("c_backtrace::named_frame_present")
}

#[inline(never)]
fn retained_capture() -> Backtrace {
    black_box(Backtrace::force_capture())
}

fn main() {
    let bt = Backtrace::force_capture();
    assert!(
        matches!(bt.status(), BacktraceStatus::Captured),
        "backtrace not captured"
    );
    let shallow = deep(0);
    let deeper = deep(30);
    assert!(shallow >= 1, "captured 0 frames");
    assert!(
        deeper >= shallow + 30,
        "recursion depth is not reflected in shadow frames: {shallow} -> {deeper}"
    );
    assert!(named_frame_present(), "guest function name is not symbolized by the standard backtrace");
    let retained = retained_capture();
    let workers: Vec<_> = (0..4)
        .map(|_| std::thread::spawn(|| format!("{:?}", retained_capture())))
        .collect();
    for worker in workers {
        assert!(worker.join().unwrap().contains("c_backtrace::retained_capture"));
    }
    assert!(format!("{retained:?}").contains("c_backtrace::retained_capture"));
    println!("backtrace: captured, named, non-empty, depth reflected (+30 frames)");
}
