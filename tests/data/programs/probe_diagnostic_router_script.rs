#!/usr/bin/env mirvm
---
[dependencies]
---

fn router_unused_script() {}

unsafe extern "C" {
    fn write(fd: i32, bytes: *const u8, len: usize) -> isize;
    fn mirvm_diagnostic_router_missing_script();
}

const GUEST_STDERR: &[u8] =
    b"warning: 1 warning emitted\nrouter-guest-binary-script:\0\xff\x1b[31m\n";

fn main() {
    unsafe {
        let _ = write(2, GUEST_STDERR.as_ptr(), GUEST_STDERR.len());
        mirvm_diagnostic_router_missing_script();
    }
}
