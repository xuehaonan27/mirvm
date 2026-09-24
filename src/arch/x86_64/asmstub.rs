//! x86_64 machine-code byte emission and single-instruction primitives.
//!
//! Holds the codearena entry-stub byte factory (`movabs rax, target; jmp rax`)
//! and the interpreter's `asm!` site (the int3 breakpoint). Emission
//! and execution only: the stub's address-region semantics (the pair's `addrspace`) and the
//! breakpoint's termination semantics (same as native) stay with the caller.
//!
//! `mirvm_syscall_trampoline` is where a `syscall` instruction inside an asm
//! stub is rewritten to land. It preserves the full syscall contract (a real
//! syscall clobbers only rcx/r11, never flags or vector state, and the wrapper
//! must keep the same discipline) before calling `mirvm_syscall_dispatch`.

/// Stub bytes: `movabs rax, target; jmp rax` (48 B8 <imm64> FF E0), 12B long.
/// (`STUB_STRIDE = 16` is this 12B rounded up; the constant lives in
/// `vm/codearena`.)
pub fn emit_stub_bytes(target: u64) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0] = 0x48;
    b[1] = 0xb8;
    b[2..10].copy_from_slice(&target.to_le_bytes());
    b[10] = 0xff;
    b[11] = 0xe0;
    b
}

/// Stub bytes: `movabs rcx, argument; movabs rax, target; jmp rax`
/// (48 B9 <imm64> 48 B8 <imm64> FF E0), 22B long.
///
/// The System V argument order leaves `rcx` as the fourth integer register, so this is the shape
/// a tail jump takes when the entry must also carry one argument the kernel did not supply. Which
/// entry needs that — a Linux SA_SIGINFO handler arrives with (signum, siginfo, ucontext) in
/// rdi/rsi/rdx — is the kernel's contract and belongs to `os_arch`; this function only encodes the
/// instruction.
pub fn emit_arg_stub_bytes(argument: u64, target: u64) -> [u8; 22] {
    let mut b = [0u8; 22];
    b[0] = 0x48;
    b[1] = 0xb9;
    b[2..10].copy_from_slice(&argument.to_le_bytes());
    b[10] = 0x48;
    b[11] = 0xb8;
    b[12..20].copy_from_slice(&target.to_le_bytes());
    b[20] = 0xff;
    b[21] = 0xe0;
    b
}

/// A real `int3`: terminates with SIGTRAP when not being traced (same as native).
pub fn int3() {
    unsafe { std::arch::asm!("int3", options(nomem, nostack, preserves_flags)) };
}

/// The register the bridge entry for `call` loads its engine into: the register the replacement's
/// trailing `owner` parameter arrives in, which the System V argument order puts after the call's
/// own arguments.
pub fn bridge_owner_register(call: &str) -> Option<&'static str> {
    match call {
        "pthread_create" => Some("r8"),
        "pthread_key_create" | "pthread_setspecific" | "signal" => Some("rdx"),
        "pthread_key_delete" | "raise" => Some("rsi"),
        "sigaction" => Some("rcx"),
        _ => None,
    }
}

/// The single-byte `ret`. This is what a `.text` slot that is never executed is filled with: an
/// image whose only job is to name addresses has to have *something* decodable there.
pub const RET: u8 = 0xc3;

/// Bytes a codearena entry-stub slot occupies: the 12-byte stub rounded up so that every slot start
/// is aligned.
pub const STUB_STRIDE: u64 = 16;

/// The contents of one inert symbol-image slot: `ret` followed by `nop` padding, so a slot start is
/// always a decodable instruction and nothing after the return is ever reached.
pub const INERT_SLOT: [u8; STUB_STRIDE as usize] = {
    let mut slot = [0x90u8; STUB_STRIDE as usize];
    slot[0] = RET;
    slot
};

// ===== syscall interception trampoline =====
//
// Contract (bit-for-bit identical to a real syscall):
// - In: rax=nr, rdi/rsi/rdx/r10/r8/r9=a1..a6; out: rax=return value.
// - A real syscall clobbers only rcx/r11 and never touches flags or xmm/mxcsr.
//   `call mirvm_syscall_dispatch` (an ordinary SysV function) clobbers far
//   more, so the trampoline preserves everything. The dispatch body itself
//   leaves vector state alone, but TRACE printing does not, and unconditional
//   preservation is the honest discipline.
// - Stack: the 6 argument slots double as dispatch's args array
//   (*(rsp)=a1 .. *(rsp+40)=a6).
//
// Stack alignment accounting: on entry rsp≡8 (call pushed the return address);
// pushfq -> 0; sub 272 -> 0; 6 pushes -> 0 (48 ≡ 0 mod 16); so rsp≡0 at `call`.

unsafe extern "C" {
    fn mirvm_syscall_trampoline();
}

/// Get trampoline address, which would be filled into asm-stub/global-asm
pub fn syscall_trampoline_addr() -> u64 {
    mirvm_syscall_trampoline as *const () as u64
}

// TODO:
// 1. movdqu only save lower 128 bits, AVX/AVX-512 will crash (e.g. taget-cpu=native)
// 2. using xsave/xsavec to save?
// 3. common VM/sandbox would use such structure to hook syscall as well, mirvm might
// integrate with VM/sandbox.
std::arch::global_asm!(
    ".globl mirvm_syscall_trampoline", // global symbol
    ".p2align 4",                      // align to 16 bytes
    "mirvm_syscall_trampoline:",       // declare entry
    "pushfq",                          // save RFLAGS (including DF, CF/ZF/SF)
    "sub rsp, 272",                    // save all SSE state to stack, 16 * xmm + 16 = = 272
    // `mirvm_syscall_dispatch` does not guarantee anything
    "movdqu [rsp], xmm0",
    "movdqu [rsp+16], xmm1",
    "movdqu [rsp+32], xmm2",
    "movdqu [rsp+48], xmm3",
    "movdqu [rsp+64], xmm4",
    "movdqu [rsp+80], xmm5",
    "movdqu [rsp+96], xmm6",
    "movdqu [rsp+112], xmm7",
    "movdqu [rsp+128], xmm8",
    "movdqu [rsp+144], xmm9",
    "movdqu [rsp+160], xmm10",
    "movdqu [rsp+176], xmm11",
    "movdqu [rsp+192], xmm12",
    "movdqu [rsp+208], xmm13",
    "movdqu [rsp+224], xmm14",
    "movdqu [rsp+240], xmm15",
    "stmxcsr [rsp+256]", // SSE control/status register
    // Here: still 16 bytes aligned.
    // rax: holding syscall number, do not need saving.
    // rcx, r11: to be destroyed by syscall according to ISA.
    // rbx, rbp, r12-r15: callee-saved registers.
    "push r9",
    "push r8",
    "push r10",
    "push rdx",
    "push rsi",
    "push rdi",
    "mov rdi, rax", // syscall number
    "mov rsi, rsp", // syscall arguments
    "call mirvm_syscall_dispatch",
    // rax should not be touched since it's holding return value.
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop r10",
    "pop r8",
    "pop r9",
    "ldmxcsr [rsp+256]",
    "movdqu xmm0, [rsp+0]",
    "movdqu xmm1, [rsp+16]",
    "movdqu xmm2, [rsp+32]",
    "movdqu xmm3, [rsp+48]",
    "movdqu xmm4, [rsp+64]",
    "movdqu xmm5, [rsp+80]",
    "movdqu xmm6, [rsp+96]",
    "movdqu xmm7, [rsp+112]",
    "movdqu xmm8, [rsp+128]",
    "movdqu xmm9, [rsp+144]",
    "movdqu xmm10, [rsp+160]",
    "movdqu xmm11, [rsp+176]",
    "movdqu xmm12, [rsp+192]",
    "movdqu xmm13, [rsp+208]",
    "movdqu xmm14, [rsp+224]",
    "movdqu xmm15, [rsp+240]",
    "add rsp, 272",
    "popfq",
    "ret",
);
