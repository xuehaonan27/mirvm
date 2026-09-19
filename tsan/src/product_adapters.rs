//! Pure-Rust product leaves required by the independently compiled VM harness.

pub(crate) mod sysroot {
    use std::path::PathBuf;

    pub(crate) fn cache_dir() -> PathBuf {
        if let Some(path) = std::env::var_os("MIRVM_HOME") {
            return PathBuf::from(path);
        }
        let home = std::env::var_os("HOME").expect("HOME is not set");
        PathBuf::from(home).join(".mirvm")
    }
}

pub(crate) mod lower {
    pub(crate) mod asm {
        pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
            let mut hash = 0xcbf2_9ce4_8422_2325u64;
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash
        }

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
