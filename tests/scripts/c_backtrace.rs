#!/usr/bin/env mirvm
---
---
// Guest backtrace shadow frames. A backtrace's **exact text** is not well-defined (addresses
// vary per run, symbol names depend on debuginfo), so the oracle uses invariants: (1) capture
// succeeds (status=Captured) (2) non-empty (3) **depth is reflected truthfully** (deeper call
// site -> more frames). native and mirvm both satisfy these and print the same deterministic
// line; mirvm's synthesized IPs stay honest <unknown> and are never faked, so text is not diffed.
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

fn named_frame_present() -> bool {
    format!("{:?}", Backtrace::force_capture()).contains("c_backtrace::deep")
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
    println!("backtrace: captured, named, non-empty, depth reflected (+30 frames)");
}
