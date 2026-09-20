//! Pure-Rust product leaves required by the independently compiled VM harness.

pub(crate) mod lower {
    pub(crate) mod asm {
        pub(crate) fn refill_syscall_slot(handle: usize) {
            let slot = crate::os::dll::sym(handle, c"mirvm_syscall_slot");
            if slot != 0 {
                unsafe {
                    *(slot as *mut u64) = crate::arch::x86_64::asmstub::syscall_trampoline_addr();
                }
            }
        }
    }
}
