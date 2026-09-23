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

/// The assembly text of a jump through the hidden 8-byte slot `slot`, without a trailing newline.
///
/// This is the body of a P1 entry trampoline: the rlib-rescue path defines one such trampoline for
/// each symbol a native archive left undefined and the crate graph exports, and the owning Engine
/// writes the address the trampoline should reach into the slot. Which symbol needs one, and the
/// directives naming it, belong to the object format; the instruction is the CPU's.
///
/// The caller must already have selected this architecture's syntax
/// (`crate::arch::asm_text::DIRECTIVE_INTEL`).
pub fn indirect_jump_asm(slot: &str) -> String {
    format!("    jmp QWORD PTR [rip + {slot}]")
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

/// The bridge entries for the calls `crate::vm::interpose` lists: this architecture's machine code,
/// reached by name because the platform's linker was asked to redirect those calls.
///
/// Each entry loads the owning engine into the argument register the replacement's trailing `owner`
/// parameter occupies -- the owner is always the last parameter, so its register is the count of the
/// call's own arguments -- and jumps indirect through the slot
/// `crate::vm::native_instance::wire` fills. A call's own arguments are never touched, and the jump
/// leaves the return address the caller pushed in place, so the replacement returns to the caller.
pub const NATIVE_RUNTIME_BRIDGE_ASM: &str = r#"
.intel_syntax noprefix
.text
.p2align 4
.globl __wrap_pthread_create
.hidden __wrap_pthread_create
.type __wrap_pthread_create,@function
__wrap_pthread_create:
    mov r8, QWORD PTR [rip + __mirvm_pthread_owner]
    jmp QWORD PTR [rip + __mirvm_pthread_create_target]
.size __wrap_pthread_create,.-__wrap_pthread_create

.p2align 4
.globl __wrap_pthread_key_create
.hidden __wrap_pthread_key_create
.type __wrap_pthread_key_create,@function
__wrap_pthread_key_create:
    mov rdx, QWORD PTR [rip + __mirvm_pthread_owner]
    jmp QWORD PTR [rip + __mirvm_pthread_key_create_target]
.size __wrap_pthread_key_create,.-__wrap_pthread_key_create

.p2align 4
.globl __wrap_pthread_setspecific
.hidden __wrap_pthread_setspecific
.type __wrap_pthread_setspecific,@function
__wrap_pthread_setspecific:
    mov rdx, QWORD PTR [rip + __mirvm_pthread_owner]
    jmp QWORD PTR [rip + __mirvm_pthread_setspecific_target]
.size __wrap_pthread_setspecific,.-__wrap_pthread_setspecific

.p2align 4
.globl __wrap_pthread_key_delete
.hidden __wrap_pthread_key_delete
.type __wrap_pthread_key_delete,@function
__wrap_pthread_key_delete:
    mov rsi, QWORD PTR [rip + __mirvm_pthread_owner]
    jmp QWORD PTR [rip + __mirvm_pthread_key_delete_target]
.size __wrap_pthread_key_delete,.-__wrap_pthread_key_delete

.p2align 4
.globl __wrap_signal
.hidden __wrap_signal
.type __wrap_signal,@function
__wrap_signal:
    mov rdx, QWORD PTR [rip + __mirvm_signal_owner]
    jmp QWORD PTR [rip + __mirvm_signal_target]
.size __wrap_signal,.-__wrap_signal

.p2align 4
.globl __wrap_sigaction
.hidden __wrap_sigaction
.type __wrap_sigaction,@function
__wrap_sigaction:
    mov rcx, QWORD PTR [rip + __mirvm_signal_owner]
    jmp QWORD PTR [rip + __mirvm_sigaction_target]
.size __wrap_sigaction,.-__wrap_sigaction

.p2align 4
.globl __wrap_raise
.hidden __wrap_raise
.type __wrap_raise,@function
__wrap_raise:
    mov rsi, QWORD PTR [rip + __mirvm_signal_owner]
    jmp QWORD PTR [rip + __mirvm_raise_target]
.size __wrap_raise,.-__wrap_raise

.pushsection .data.mirvm_pthread,"aw",@progbits
.p2align 3
.globl __mirvm_pthread_owner
.hidden __mirvm_pthread_owner
.type __mirvm_pthread_owner,@object
.size __mirvm_pthread_owner,8
__mirvm_pthread_owner:
    .quad 0
.globl __mirvm_pthread_create_target
.hidden __mirvm_pthread_create_target
.type __mirvm_pthread_create_target,@object
.size __mirvm_pthread_create_target,8
__mirvm_pthread_create_target:
    .quad 0
.globl __mirvm_pthread_key_create_target
.hidden __mirvm_pthread_key_create_target
.type __mirvm_pthread_key_create_target,@object
.size __mirvm_pthread_key_create_target,8
__mirvm_pthread_key_create_target:
    .quad 0
.globl __mirvm_pthread_setspecific_target
.hidden __mirvm_pthread_setspecific_target
.type __mirvm_pthread_setspecific_target,@object
.size __mirvm_pthread_setspecific_target,8
__mirvm_pthread_setspecific_target:
    .quad 0
.globl __mirvm_pthread_key_delete_target
.hidden __mirvm_pthread_key_delete_target
.type __mirvm_pthread_key_delete_target,@object
.size __mirvm_pthread_key_delete_target,8
__mirvm_pthread_key_delete_target:
    .quad 0
.popsection

.pushsection .data.mirvm_signal,"aw",@progbits
.p2align 3
.globl __mirvm_signal_owner
.hidden __mirvm_signal_owner
.type __mirvm_signal_owner,@object
.size __mirvm_signal_owner,8
__mirvm_signal_owner:
    .quad 0
.globl __mirvm_signal_target
.hidden __mirvm_signal_target
.type __mirvm_signal_target,@object
.size __mirvm_signal_target,8
__mirvm_signal_target:
    .quad 0
.globl __mirvm_sigaction_target
.hidden __mirvm_sigaction_target
.type __mirvm_sigaction_target,@object
.size __mirvm_sigaction_target,8
__mirvm_sigaction_target:
    .quad 0
.globl __mirvm_raise_target
.hidden __mirvm_raise_target
.type __mirvm_raise_target,@object
.size __mirvm_raise_target,8
__mirvm_raise_target:
    .quad 0
.popsection
.section .note.GNU-stack,"",@progbits
"#;
