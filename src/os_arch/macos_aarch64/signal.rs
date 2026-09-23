//! macOS kernel signal ABI as aarch64 encodes it.
//!
//! `os/macos/signal.rs` holds this platform's half of sigaction handling — the mask, the flags, the
//! `libc::sigaction` wrapper and its query/install paths — and stays CPU-neutral. What is left here
//! is the part where the kernel and the CPU meet: the bytes a fixed entry stub is made of, the
//! `ucontext_t` fields the debug dump reads, and the restorer half.
//!
//! The restorer half is where this platform differs from the other one, and it differs by having
//! nothing to say. Linux reads the handler's return path out of the action itself, so it has a
//! restorer slot and a flag marking it; this kernel places a trampoline on the stack instead and
//! its `sigaction` has no such field (Darwin's is `{ sa_sigaction, sa_mask, sa_flags }`). The
//! functions below therefore keep their shape and answer with the absence: the flag is `0`, which
//! makes every mask-off comparison in `os/macos/signal.rs` a no-op, and the ones whose job is to
//! read or write the slot do nothing because there is no slot.

/// This kernel's actions carry no restorer, so there is no flag marking one.
pub const RESTORER_FLAG: i32 = 0;

/// Bytes a fixed signal-entry stub occupies on this pair.
///
/// The stub is [`crate::arch::asmstub::emit_arg_stub_bytes`]: two literal loads, the branch through
/// the second, and both literals.
pub const ENTRY_STUB_SIZE: usize = 28;

/// The bytes of a fixed SA_SIGINFO entry stub. The kernel enters the handler with
/// (signum, siginfo, ucontext) in x0/x1/x2, so the stub supplies the adapter's own argument as the
/// fourth and branches: the kernel's stack stays exactly where its own return path expects it.
pub fn entry_stub_bytes(argument: usize, adapter: usize) -> [u8; ENTRY_STUB_SIZE] {
    crate::arch::asmstub::emit_arg_stub_bytes(argument as u64, adapter as u64)
}

/// Nothing to point anywhere, for the same reason as the restorer slot's absence.
pub fn set_runtime_restorer(_action: &mut libc::sigaction) {}

/// Never: no action this kernel returns can carry MIRVM's restorer.
pub fn uses_runtime_restorer(_action: &libc::sigaction) -> bool {
    false
}

/// Restore a disposition previously read from the kernel.
///
/// There is no raw request layout to reproduce here, because `libc::sigaction` *is* this kernel's
/// structure, so libc's own call is already exact.
pub fn replace_exact(action: &libc::sigaction, signum: i32) -> Result<libc::sigaction, i32> {
    let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::sigaction(signum, action, &mut old) };
    if result == 0 {
        Ok(old)
    } else {
        Err(crate::os::process::errno())
    }
}

/// Always true: with no restorer slot, two actions cannot disagree about one.
pub fn same_restorer(_left: &libc::sigaction, _right: &libc::sigaction) -> bool {
    true
}

/// Always true: an install cannot have changed a restorer this kernel does not keep.
pub fn restorer_matches(_actual: &libc::sigaction, _requested: &libc::sigaction) -> bool {
    true
}

/// Translate a known kernel-stub action back to its caller-visible form. Keep mask and ordinary
/// flag changes made to a raw oldact, but remove the adapter ABI bits that were not in `visible`.
pub fn canonicalize_from_kernel_stub(
    action: &mut libc::sigaction,
    kernel: &libc::sigaction,
    visible: &libc::sigaction,
) {
    action.sa_sigaction = visible.sa_sigaction;
    action.sa_flags &= !(kernel.sa_flags & !visible.sa_flags);
}

// ===== the debug dump's view of the kernel frame =====
//
// `libc` carries no `ucontext_t` on this platform, so what the dump needs is declared here. The
// layouts are Darwin's, read out of the SDK's `sys/ucontext.h` and `mach/arm/_structs.h`, and the
// offsets are pinned by the test at the bottom of this file rather than trusted: a
// `__darwin_mcontext64` is 816 bytes, of which the dump reads the first 288.

/// Darwin's `__darwin_arm_exception_state64`, whose `__far` holds the fault address.
#[repr(C)]
struct ExceptionState64 {
    far: u64,
    esr: u32,
    exception: u32,
}

/// Darwin's `__darwin_arm_thread_state64`, whose `__pc` holds the faulting instruction.
#[repr(C)]
struct ThreadState64 {
    x: [u64; 29],
    fp: u64,
    lr: u64,
    sp: u64,
    pc: u64,
    cpsr: u32,
    _pad: u32,
}

/// Darwin's `__darwin_mcontext64`, which a `ucontext_t` points at.
#[repr(C)]
struct Mcontext64 {
    es: ExceptionState64,
    ss: ThreadState64,
}

/// Darwin's `__darwin_sigaltstack`.
#[repr(C)]
struct Sigaltstack {
    sp: *mut libc::c_void,
    size: usize,
    flags: i32,
    _pad: i32,
}

/// Darwin's `ucontext_t`, which is what the kernel passes a handler and which therefore cannot be
/// made opaque. Only the fields up to `uc_mcontext` are named, because only those are read.
#[repr(C)]
struct Ucontext {
    onstack: i32,
    sigmask: u32,
    stack: Sigaltstack,
    link: *mut Ucontext,
    mcsize: usize,
    mcontext: *mut Mcontext64,
}

/// MIRVM_SEGV_DUMP troubleshooting hook: installs an SA_SIGINFO handler that prints the faulting
/// instruction and the fault address and then exits.
///
/// The Linux half of this hook also names the owning mapping and dumps the segment, which it can do
/// from `/proc/self/maps`. This platform has neither that file nor a `vm_region` binding in `libc`,
/// and this port does not declare one for a troubleshooting hook, so the two addresses are what the
/// dump gives.
pub fn install_segv_dump() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = segv_dump_handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
    }
}

unsafe extern "C" fn segv_dump_handler(
    _sig: i32,
    _info: *mut libc::siginfo_t,
    ctx: *mut libc::c_void,
) {
    unsafe {
        let uc = ctx as *mut Ucontext;
        let mc = (*uc).mcontext;
        let pc = (*mc).ss.pc as usize;
        let addr = (*mc).es.far as usize;
        eprintln!("mirvm-segv-dump: fault addr(FAR)={addr:#x} pc={pc:#x}");
        std::process::exit(134);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    /// The offsets the dump's reads depend on, as `sys/ucontext.h` and `mach/arm/_structs.h`
    /// define them for this platform. Pinned because nothing else would notice a `repr(C)`
    /// declaration that no longer matches the header.
    #[test]
    fn the_declared_layout_matches_darwins() {
        assert_eq!(offset_of!(Ucontext, sigmask), 4);
        assert_eq!(offset_of!(Ucontext, mcontext), 48);
        assert_eq!(offset_of!(Mcontext64, es), 0);
        assert_eq!(offset_of!(Mcontext64, ss), 16);
        assert_eq!(offset_of!(Mcontext64, es.far), 0);
        assert_eq!(offset_of!(Mcontext64, ss.pc), 272);
        assert_eq!(size_of::<Mcontext64>(), 288);
    }
}
