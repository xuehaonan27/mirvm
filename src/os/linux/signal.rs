//! Linux signal primitives.
//! - signal/sigaction passthrough
//! - sigaction structure copy for handler read/write
//! - SEGV troubleshooting dump installation
//!
//! Primitives are not adjudicated:
//! - Guest handler whitelist (sync fault signal rejection)
//! - AS-trampoline materialization
//! - DFL/IGN decision
//!
//! These decisions are all left to the engine. This module only handles kernel
//! calls and structure layout knowledge.

pub const SIG_DFL: usize = libc::SIG_DFL;
pub const SIG_IGN: usize = libc::SIG_IGN;
pub const SIG_ERR: usize = usize::MAX;

// Linux/x86_64 libc adds this kernel restorer detail while installing an
// action. It is not part of the caller-requested disposition.
const SA_RESTORER: i32 = 0x0400_0000;
const SA_EXPOSE_TAGBITS: i32 = 0x0000_0800;

#[cfg(target_arch = "x86_64")]
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

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn mirvm_signal_restorer();
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
struct KernelSigaction {
    handler: usize,
    flags: usize,
    restorer: usize,
    mask: u64,
}

// Linux promises that these long-established flags survive rt_sigaction.
// SA_UNSUPPORTED and future probing bits are deliberately excluded: a current
// kernel may clear them while reporting which optional bits it understands.
const STABLE_KERNEL_FLAGS: i32 = libc::SA_NOCLDSTOP
    | libc::SA_NOCLDWAIT
    | libc::SA_SIGINFO
    | libc::SA_ONSTACK
    | libc::SA_RESTART
    | libc::SA_NODEFER
    | libc::SA_RESETHAND
    | SA_EXPOSE_TAGBITS;

// Synchronous fault signals.
pub const SIGSEGV: i32 = libc::SIGSEGV;
pub const SIGBUS: i32 = libc::SIGBUS;
pub const SIGFPE: i32 = libc::SIGFPE;
pub const SIGILL: i32 = libc::SIGILL;
pub const SIGTRAP: i32 = libc::SIGTRAP;

/// Upper bound on Linux's traditional (non-realtime) signal numbers. Realtime
/// signals carry queueing and siginfo semantics and cannot be folded into a VM
/// mailbox that merges only by signal number.
pub const STANDARD_SIGNAL_MAX: i32 = 31;

pub fn is_realtime(signum: i32) -> bool {
    signum >= libc::SIGRTMIN() && signum <= libc::SIGRTMAX()
}

/// A sigaction structure (layout knowledge encapsulated). The engine edits a
/// copy's handler and writes it back to the kernel; the original guest structure
/// stays untouched because the guest may reuse or read it back.
#[derive(Clone, Copy)]
pub struct Sigaction(libc::sigaction);

/// Restores the calling host thread's real signal mask after a deferred
/// handler has finished. Guest handlers run in ordinary VM state, but native
/// dispositions on the same thread must still observe POSIX `sa_mask`.
pub struct ThreadSignalMaskGuard {
    previous: libc::sigset_t,
}

impl Drop for ThreadSignalMaskGuard {
    fn drop(&mut self) {
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
        };
        if result != 0 {
            eprintln!("mirvm[m4-engine]: failed to restore host signal mask: {result}");
            std::process::abort();
        }
    }
}

impl Sigaction {
    fn same_mask(&self, other: &Self) -> bool {
        (1..=libc::SIGRTMAX())
            .filter(|&signum| signum != libc::SIGKILL && signum != libc::SIGSTOP)
            .all(|signum| unsafe {
                libc::sigismember(&self.0.sa_mask, signum)
                    == libc::sigismember(&other.0.sa_mask, signum)
            })
    }

    /// Copies from a guest-side `act` pointer (pointer 0 -> None, matching the
    /// kernel's act=NULL semantics).
    ///
    /// # Safety
    /// When `ptr` is non-zero it must point to a complete sigaction structure in
    /// the guest address space (true address model).
    pub unsafe fn copy_from(ptr: u64) -> Option<Self> {
        if ptr == 0 {
            return None;
        }
        Some(Sigaction(unsafe { *(ptr as *const libc::sigaction) }))
    }

    pub fn handler(&self) -> usize {
        self.0.sa_sigaction
    }

    /// Standard signals currently blocked by the calling pthread. MIRVM uses
    /// this together with its logical deferred-handler mask before choosing a
    /// safe-point delivery.
    pub fn current_standard_mask_bits() -> Result<u64, i32> {
        let mut current: libc::sigset_t = unsafe { std::mem::zeroed() };
        let result =
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut current) };
        if result != 0 {
            return Err(result);
        }
        let mut bits = 0u64;
        for signum in 1..=STANDARD_SIGNAL_MAX {
            if unsafe { libc::sigismember(&current, signum) } == 1 {
                bits |= 1u64 << signum;
            }
        }
        Ok(bits)
    }

    /// Linux silently removes the two unmaskable signals from `sa_mask`.
    /// Keep the guest-visible copy identical to the disposition the kernel
    /// actually accepted instead of remembering impossible mask bits.
    pub fn normalized_for_kernel(mut self) -> Self {
        unsafe {
            libc::sigdelset(&mut self.0.sa_mask, libc::SIGKILL);
            libc::sigdelset(&mut self.0.sa_mask, libc::SIGSTOP);
        }
        self
    }

    pub fn has_unsupported_guest_flags(&self) -> bool {
        self.0.sa_flags
            & (libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_NODEFER | libc::SA_RESETHAND)
            != 0
    }

    pub fn flags(&self) -> i32 {
        self.0.sa_flags
    }

    pub fn empty(handler: usize, flags: i32) -> Self {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler;
        action.sa_flags = flags;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        Self(action)
    }

    /// glibc `signal()` uses BSD-style persistent handlers with restartable
    /// syscalls. The kernel itself adds `signum` to the temporary mask.
    pub fn for_signal(handler: usize) -> Self {
        Self::empty(handler, libc::SA_RESTART)
    }

    /// Keep the guest-visible mask and supported flags, but route the kernel
    /// frame to a MIRVM-owned SA_SIGINFO stub. Guest SA_SIGINFO itself is
    /// rejected before this conversion.
    pub fn for_kernel_stub(mut self, handler: usize) -> Self {
        self.0.sa_sigaction = handler;
        self.0.sa_flags |= libc::SA_SIGINFO;
        self
    }

    /// Give an internal fixed-stub action an exact restorer identity. Public
    /// native dispositions still go through libc unchanged.
    pub fn with_runtime_restorer(mut self) -> Self {
        #[cfg(target_arch = "x86_64")]
        if !matches!(self.handler(), SIG_DFL | SIG_IGN) {
            self.0.sa_flags |= SA_RESTORER;
            self.0.sa_restorer = Some(unsafe {
                std::mem::transmute::<unsafe extern "C" fn(), extern "C" fn()>(
                    mirvm_signal_restorer,
                )
            });
        }
        self
    }

    /// Translate a known kernel-stub action back to its caller-visible form.
    /// Keep mask and ordinary flag changes made to a raw oldact, but remove the
    /// adapter ABI bits and libc restorer state that were not in `visible`.
    pub fn canonicalized_from_kernel_stub(mut self, kernel: &Self, visible: &Self) -> Self {
        self.0.sa_sigaction = visible.handler();
        let added_flags = (kernel.flags() & !visible.flags()) | (SA_RESTORER & !visible.flags());
        self.0.sa_flags &= !added_flags;
        self.0.sa_restorer = visible.0.sa_restorer;
        self
    }

    pub fn standard_mask_bits(&self) -> u64 {
        let mut bits = 0u64;
        for signum in 1..=STANDARD_SIGNAL_MAX {
            if unsafe { libc::sigismember(&self.0.sa_mask, signum) } == 1 {
                bits |= 1u64 << signum;
            }
        }
        bits
    }

    /// Apply this action's mask plus the delivered signal to the real host
    /// thread while a deferred handler runs. This prevents a genuine native
    /// disposition from interrupting the handler contrary to POSIX `sa_mask`.
    pub fn block_for_handler(&self, signum: i32) -> Result<ThreadSignalMaskGuard, i32> {
        let mut blocked = self.0.sa_mask;
        unsafe { libc::sigaddset(&mut blocked, signum) };
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous) };
        if result == 0 {
            Ok(ThreadSignalMaskGuard { previous })
        } else {
            Err(result)
        }
    }

    pub fn query(signum: i32) -> Result<Self, i32> {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::sigaction(signum, std::ptr::null(), &mut action) };
        if result == 0 {
            Ok(Self(action))
        } else {
            Err(crate::os::process::errno())
        }
    }

    #[cfg(test)]
    pub fn install(&self, signum: i32) -> i32 {
        unsafe { libc::sigaction(signum, &self.0, std::ptr::null_mut()) }
    }

    /// Install a caller request and return the action that immediately
    /// preceded it. External requests go through libc so they keep libc's
    /// normal `sigaction` semantics. MIRVM's own fixed-stub action already
    /// carries an exact kernel restorer and therefore uses `rt_sigaction`.
    pub fn replace(&self, signum: i32) -> Result<Self, i32> {
        #[cfg(target_arch = "x86_64")]
        if self.uses_runtime_restorer() {
            return self.replace_exact_kernel(signum);
        }

        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::sigaction(signum, &self.0, &mut old) };
        if result == 0 {
            Ok(Self(old))
        } else {
            Err(crate::os::process::errno())
        }
    }

    /// Restore a disposition previously read from the kernel without letting
    /// libc replace its flags or restorer. This is for old-action snapshots,
    /// never for a fresh guest/native request.
    pub fn replace_exact(&self, signum: i32) -> Result<Self, i32> {
        #[cfg(target_arch = "x86_64")]
        {
            self.replace_exact_kernel(signum)
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            let result = unsafe { libc::sigaction(signum, &self.0, &mut old) };
            if result == 0 {
                Ok(Self(old))
            } else {
                Err(crate::os::process::errno())
            }
        }
    }

    pub fn write_to(&self, ptr: u64) {
        if ptr != 0 {
            unsafe { (ptr as *mut libc::sigaction).write(self.0) };
        }
    }

    pub fn same_disposition(&self, other: &Self) -> bool {
        self.handler() == other.handler()
            && self.flags() == other.flags()
            && Self::same_restorer(self.0.sa_restorer, other.0.sa_restorer)
            && self.same_mask(other)
    }

    fn same_restorer(left: Option<extern "C" fn()>, right: Option<extern "C" fn()>) -> bool {
        match (left, right) {
            (Some(left), Some(right)) => std::ptr::fn_addr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn uses_runtime_restorer(&self) -> bool {
        let runtime: unsafe extern "C" fn() = mirvm_signal_restorer;
        self.0.sa_restorer.is_some_and(|candidate| {
            candidate as usize == runtime as usize && self.flags() & SA_RESTORER != 0
        })
    }

    #[cfg(target_arch = "x86_64")]
    fn replace_exact_kernel(&self, signum: i32) -> Result<Self, i32> {
        let mut mask = 0u64;
        for member in 1..=64 {
            if unsafe { libc::sigismember(&self.0.sa_mask, member) } == 1 {
                mask |= 1u64 << (member - 1);
            }
        }
        let requested = KernelSigaction {
            handler: self.handler(),
            flags: self.flags() as u32 as usize,
            restorer: self.0.sa_restorer.map_or(0, |restorer| restorer as usize),
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
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = old.handler;
        action.sa_flags = old.flags as i32;
        action.sa_restorer = if old.restorer == 0 {
            None
        } else {
            Some(unsafe { std::mem::transmute::<usize, extern "C" fn()>(old.restorer) })
        };
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        for member in 1..=64 {
            if old.mask & (1u64 << (member - 1)) != 0 {
                unsafe { libc::sigaddset(&mut action.sa_mask, member) };
            }
        }
        Ok(Self(action))
    }

    /// Compare a post-install query with the action requested through libc.
    /// libc adds SA_RESTORER and a restorer pointer on x86_64.
    #[cfg(test)]
    pub fn satisfies_request(&self, requested: &Self) -> bool {
        self.handler() == requested.handler()
            && (self.flags() & !SA_RESTORER) == (requested.flags() & !SA_RESTORER)
            && self.same_mask(requested)
    }

    /// Whether an action returned through `oldact` can be the exact kernel
    /// result of installing `requested`. Linux may add SA_RESTORER and may
    /// clear SA_UNSUPPORTED or unknown probing bits, while its established
    /// flags and the normalized mask must remain intact.
    pub fn is_kernel_normalization_of(&self, requested: &Self) -> bool {
        let actual_flags = self.flags() & !SA_RESTORER;
        let requested_flags = requested.flags() & !SA_RESTORER;
        let requested_has_restorer = requested.flags() & SA_RESTORER != 0;
        let restorer_matches = if requested_has_restorer && requested.uses_runtime_restorer() {
            self.flags() & SA_RESTORER != 0 && Self::is_rt_sigreturn(self.0.sa_restorer)
        } else if requested_has_restorer {
            self.flags() & SA_RESTORER != 0
                && Self::same_restorer(self.0.sa_restorer, requested.0.sa_restorer)
        } else {
            self.flags() & SA_RESTORER != 0 && Self::is_rt_sigreturn(self.0.sa_restorer)
        };
        self.handler() == requested.handler()
            && self.same_mask(requested)
            && restorer_matches
            && actual_flags & !requested_flags == 0
            && actual_flags & STABLE_KERNEL_FLAGS == requested_flags & STABLE_KERNEL_FLAGS
    }

    /// Validate libc's hidden restorer by its only relevant behaviour rather
    /// than accepting an arbitrary non-null pointer. `process_vm_readv` reads
    /// our own address space without risking a SIGSEGV on a corrupt raw action.
    #[cfg(target_arch = "x86_64")]
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
        let code = if code.starts_with(&[0xf3, 0x0f, 0x1e, 0xfa]) {
            &code[4..]
        } else {
            &code[..]
        };
        code.starts_with(&[0x48, 0xc7, 0xc0, 0x0f, 0, 0, 0, 0x0f, 0x05])
            || code.starts_with(&[0xb8, 0x0f, 0, 0, 0, 0x0f, 0x05])
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn is_rt_sigreturn(_restorer: Option<extern "C" fn()>) -> bool {
        false
    }
}

/// MIRVM_SEGV_DUMP troubleshooting hook: installs an SA_SIGINFO handler that
/// prints the fault RIP (the ucontext RIP field, x86_64 = gregs[REG_RIP=16]), the
/// fault address (CR2 = gregs[22]) and their /proc/self/maps owners, dumps the
/// owning executable segment to /tmp/mirvm-jitdump.bin (objdump it to find the
/// crash site), and then exits.
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
