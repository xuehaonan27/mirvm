//! Linux kernel signal ABI as x86_64 encodes it.
//!
//! `os/linux/signal.rs` holds the Linux half of sigaction handling — the mask, the flags, the
//! `libc::sigaction` wrapper and its query/install paths — and stays CPU-neutral. What is left
//! here is the part where the kernel and the CPU meet:
//!
//! - `SA_RESTORER` and the restorer slot: x86_64 is the architecture on which Linux reads the
//!   return path out of the action itself, so glibc adds a hidden restorer pointer that is not
//!   part of the caller's request and must not leak into a guest-visible oldact.
//! - `mirvm_signal_restorer`: `rt_sigreturn` reached through the x86_64 syscall instruction.
//! - The raw `rt_sigaction` request layout, which is what a snapshot can be restored through
//!   without libc rewriting its flags or restorer.
//! - The `ucontext_t` register indices the debug dump reads.

/// The action flag that marks an explicit restorer. Linux reads the restorer out of the action
/// on this architecture; a pair whose kernel returns through a vDSO symbol instead declares
/// this `0`, which makes the mask-off comparisons in `os/linux/signal.rs` no-ops.
pub const RESTORER_FLAG: i32 = 0x0400_0000;

/// Bytes a fixed signal-entry stub occupies on this pair.
pub const ENTRY_STUB_SIZE: usize = 22;

/// The bytes of a fixed SA_SIGINFO entry stub. The kernel enters the handler with
/// (signum, siginfo, ucontext) in rdi/rsi/rdx, so the stub supplies the adapter's own argument as
/// the fourth and tail-jumps: the kernel's stack stays exactly where the restorer expects it.
pub fn entry_stub_bytes(argument: usize, adapter: usize) -> [u8; ENTRY_STUB_SIZE] {
    crate::arch::asmstub::emit_arg_stub_bytes(argument as u64, adapter as u64)
}

std::arch::global_asm!(
    ".globl mirvm_signal_restorer",
    ".hidden mirvm_signal_restorer",
    ".type mirvm_signal_restorer,@function",
    "mirvm_signal_restorer:",
    "mov rax, 15",
    "syscall",
    "ud2",
    ".size mirvm_signal_restorer, .-mirvm_signal_restorer",
);

unsafe extern "C" {
    fn mirvm_signal_restorer();
}

/// The `rt_sigaction` request/oldact layout: the kernel reads the restorer from between the
/// flags and the mask, a slot glibc's `struct sigaction` does not place there. Handing
/// `libc::sigaction` to the raw syscall would therefore silently shift the mask.
#[repr(C)]
struct KernelSigaction {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

/// Put `restorer` in the action's restorer slot.
///
/// The slot is the pair's, so filling it is too: a snapshot read from the kernel carries a
/// restorer here and restoring that snapshot means putting the same pointer back.
pub fn set_restorer(action: &mut libc::sigaction, restorer: extern "C" fn()) {
    action.sa_restorer = Some(restorer);
}

/// Point an internal fixed-stub action at MIRVM's own restorer, so the kernel frame returns
/// through code this process owns rather than through a libc restorer whose flags it would have
/// to negotiate. Caller-visible dispositions never take this path.
pub fn set_runtime_restorer(action: &mut libc::sigaction) {
    if matches!(action.sa_sigaction, libc::SIG_DFL | libc::SIG_IGN) {
        return;
    }
    action.sa_flags |= RESTORER_FLAG;
    let restorer = unsafe {
        std::mem::transmute::<unsafe extern "C" fn(), extern "C" fn()>(mirvm_signal_restorer)
    };
    set_restorer(action, restorer);
}

/// Whether a snapshot already carries MIRVM's restorer, which is what decides between the raw
/// syscall path and letting libc install the action.
pub fn uses_runtime_restorer(action: &libc::sigaction) -> bool {
    let runtime: unsafe extern "C" fn() = mirvm_signal_restorer;
    action.sa_restorer.is_some_and(|candidate| {
        candidate as usize == runtime as usize && action.sa_flags & RESTORER_FLAG != 0
    })
}

/// Restore a disposition previously read from the kernel without letting libc replace its flags
/// or restorer. This is for old-action snapshots, never for a fresh guest/native request.
pub fn replace_exact(action: &libc::sigaction, signum: i32) -> Result<libc::sigaction, i32> {
    let mut mask = 0u64;
    for member in 1..=64 {
        if unsafe { libc::sigismember(&action.sa_mask, member) } == 1 {
            mask |= 1u64 << (member - 1);
        }
    }
    let requested = KernelSigaction {
        handler: action.sa_sigaction,
        flags: action.sa_flags as u32 as usize,
        restorer: action.sa_restorer.map_or(0, |restorer| restorer as usize),
        mask,
    };
    let mut old = KernelSigaction {
        handler: 0,
        flags: 0,
        restorer: 0,
        mask: 0,
    };
    let result = unsafe {
        libc::syscall(
            libc::SYS_rt_sigaction,
            signum,
            &requested,
            &mut old,
            std::mem::size_of::<u64>(),
        )
    };
    if result != 0 {
        return Err(crate::os::process::errno());
    }
    let mut restored: libc::sigaction = unsafe { std::mem::zeroed() };
    restored.sa_sigaction = old.handler;
    restored.sa_flags = old.flags as i32;
    restored.sa_restorer = if old.restorer == 0 {
        None
    } else {
        Some(unsafe { std::mem::transmute::<usize, extern "C" fn()>(old.restorer) })
    };
    unsafe { libc::sigemptyset(&mut restored.sa_mask) };
    for member in 1..=64 {
        if old.mask & (1u64 << (member - 1)) != 0 {
            unsafe { libc::sigaddset(&mut restored.sa_mask, member) };
        }
    }
    Ok(restored)
}

/// Whether two actions name the same restorer.
pub fn same_restorer(left: &libc::sigaction, right: &libc::sigaction) -> bool {
    match (left.sa_restorer, right.sa_restorer) {
        (Some(left), Some(right)) => std::ptr::fn_addr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

/// Whether `actual` carries an acceptable restorer for having installed `requested`: libc's own
/// `rt_sigreturn`, an exact copy of the requested restorer, or MIRVM's restorer for a
/// fixed-stub action. Linux adds the flag and a restorer on this architecture, so an action
/// with neither can never be a faithful kernel result of a request that asked for one.
pub fn restorer_matches(actual: &libc::sigaction, requested: &libc::sigaction) -> bool {
    if actual.sa_flags & RESTORER_FLAG == 0 {
        return false;
    }
    if requested.sa_flags & RESTORER_FLAG != 0 && !uses_runtime_restorer(requested) {
        same_restorer(actual, requested)
    } else {
        is_rt_sigreturn(actual.sa_restorer)
    }
}

/// Translate a known kernel-stub action back to its caller-visible form. Keep mask and ordinary
/// flag changes made to a raw oldact, but remove the adapter ABI bits and restorer state that
/// were not in `visible`.
pub fn canonicalize_from_kernel_stub(
    action: &mut libc::sigaction,
    kernel: &libc::sigaction,
    visible: &libc::sigaction,
) {
    action.sa_sigaction = visible.sa_sigaction;
    let added_flags = (kernel.sa_flags & !visible.sa_flags) | (RESTORER_FLAG & !visible.sa_flags);
    action.sa_flags &= !added_flags;
    action.sa_restorer = visible.sa_restorer;
}

/// Validate libc's hidden restorer by its only relevant behaviour rather than accepting an
/// arbitrary non-null pointer. `process_vm_readv` reads our own address space without risking a
/// SIGSEGV on a corrupt raw action.
fn is_rt_sigreturn(restorer: Option<extern "C" fn()>) -> bool {
    let Some(restorer) = restorer else {
        return false;
    };
    let mut code = [0u8; 16];
    let local = libc::iovec {
        iov_base: code.as_mut_ptr().cast(),
        iov_len: code.len(),
    };
    let remote = libc::iovec {
        iov_base: (restorer as usize as *mut libc::c_void),
        iov_len: code.len(),
    };
    let read = unsafe { libc::process_vm_readv(libc::getpid(), &local, 1, &remote, 1, 0) };
    if read < 7 {
        return false;
    }
    // A `mov rax,15; syscall` or `mov eax,15; syscall`, optionally behind the `endbr64` that a
    // CET-enabled libc emits as its indirect-branch target.
    let code = if code.starts_with(&[0xf3, 0x0f, 0x1e, 0xfa]) {
        &code[4..]
    } else {
        &code[..]
    };
    code.starts_with(&[0x48, 0xc7, 0xc0, 0x0f, 0, 0, 0, 0x0f, 0x05])
        || code.starts_with(&[0xb8, 0x0f, 0, 0, 0, 0x0f, 0x05])
}

/// MIRVM_SEGV_DUMP troubleshooting hook: installs an SA_SIGINFO handler that prints the fault
/// RIP (the ucontext RIP field, x86_64 = gregs[REG_RIP=16]), the fault address (CR2 =
/// gregs[22]) and their /proc/self/maps owners, dumps the owning executable segment to
/// /tmp/mirvm-jitdump.bin (objdump it to find the crash site), and then exits.
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
        let uc = ctx as *mut libc::ucontext_t;
        let rip = (*uc).uc_mcontext.gregs[16] as usize; // RIP
        let addr = (*uc).uc_mcontext.gregs[22] as usize; // CR2 (the real fault address)
        eprintln!("mirvm-segv-dump: fault addr(CR2)={addr:#x} rip={rip:#x}");
        if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
            for line in maps.lines() {
                let start =
                    usize::from_str_radix(line.split('-').next().unwrap_or("0"), 16).unwrap_or(0);
                let end = usize::from_str_radix(
                    line.split_whitespace()
                        .next()
                        .unwrap_or("0-0")
                        .split('-')
                        .nth(1)
                        .unwrap_or("0"),
                    16,
                )
                .unwrap_or(0);
                if addr >= start && addr < end {
                    eprintln!("mirvm-segv-dump: fault owner: {line}");
                }
                if rip >= start && rip < end {
                    eprintln!("mirvm-segv-dump: rip owner: {line}");
                    if line.contains("xp") {
                        let bytes = std::slice::from_raw_parts(start as *const u8, end - start);
                        let _ = std::fs::write("/tmp/mirvm-jitdump.bin", bytes);
                        eprintln!(
                            "mirvm-segv-dump: executable segment dumped to /tmp/mirvm-jitdump.bin (base {start:#x})"
                        );
                    }
                }
            }
        }
        std::process::exit(134);
    }
}
