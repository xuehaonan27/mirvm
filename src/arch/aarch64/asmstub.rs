//! aarch64 machine-code byte emission and single-instruction primitives.
//!
//! Holds the codearena entry-stub byte factory and the interpreter's breakpoint. Emission and
//! execution only: the stub's address-region semantics (the pair's `addrspace`) and the
//! breakpoint's termination semantics (the same as native) stay with the caller.

/// Bytes a codearena entry-stub slot occupies.
///
/// The plain stub is exactly [`STUB_BYTES`] long — two instructions and the address they load —
/// and the arena bumps by this stride, so the two must agree.
pub const STUB_STRIDE: u64 = 16;

/// Bytes [`emit_stub_bytes`] produces: `ldr x16, #8; br x16; .quad target`.
const STUB_BYTES: usize = 16;

/// Stub bytes: `ldr x16, #8; br x16` followed by the target as a literal.
///
/// The literal load is PC-relative, so the stub is position-independent and needs no relocation
/// once it is placed: the assembler encodes the offset to the quad that follows, which is why this
/// is a byte pattern rather than an instruction sequence with a patch site.
pub fn emit_stub_bytes(target: u64) -> [u8; STUB_BYTES] {
    /// `ldr x16, #8`: load the 64-bit literal eight bytes on from this instruction.
    const LDR_X16_PC8: u32 = 0x5800_0050;
    /// `br x16`.
    const BR_X16: u32 = 0xd61f_0200;

    let mut b = [0u8; STUB_BYTES];
    b[0..4].copy_from_slice(&LDR_X16_PC8.to_le_bytes());
    b[4..8].copy_from_slice(&BR_X16.to_le_bytes());
    b[8..16].copy_from_slice(&target.to_le_bytes());
    b
}

/// Bytes [`emit_arg_stub_bytes`] produces: two literal loads, a branch, then both literals.
const ARG_STUB_BYTES: usize = 28;

/// Stub bytes: `ldr x3, #12; ldr x16, #16; br x16; .quad argument; .quad target`.
///
/// The aarch64 procedure call standard passes the first arguments in x0..x7, so x3 is the fourth,
/// and this is the shape a tail jump takes when the entry must also carry one argument the kernel
/// did not supply. Which entry needs that is the kernel's contract and belongs to `os_arch`; this
/// function only encodes the instructions.
pub fn emit_arg_stub_bytes(argument: u64, target: u64) -> [u8; ARG_STUB_BYTES] {
    /// `ldr x3, #12`: the first literal, twelve bytes on from this instruction.
    const LDR_X3_PC12: u32 = 0x5800_0063;
    /// `ldr x16, #16`: the second literal, sixteen bytes on from this instruction.
    const LDR_X16_PC16: u32 = 0x5800_0090;
    /// `br x16`.
    const BR_X16: u32 = 0xd61f_0200;

    let mut b = [0u8; ARG_STUB_BYTES];
    b[0..4].copy_from_slice(&LDR_X3_PC12.to_le_bytes());
    b[4..8].copy_from_slice(&LDR_X16_PC16.to_le_bytes());
    b[8..12].copy_from_slice(&BR_X16.to_le_bytes());
    b[12..20].copy_from_slice(&argument.to_le_bytes());
    b[20..28].copy_from_slice(&target.to_le_bytes());
    b
}

/// A real `brk #0`: terminates with SIGTRAP when not being traced (the same as native).
pub fn int3() {
    unsafe { std::arch::asm!("brk #0", options(nomem, nostack, preserves_flags)) };
}

/// The assembly text of a jump through the hidden 8-byte slot `slot`, without a trailing newline.
///
/// This is the body of a P1 entry trampoline: the rlib-rescue path defines one such trampoline for
/// each symbol a native archive left undefined and the crate graph exports, and the owning Engine
/// writes the address the trampoline should reach into the slot. Which symbol needs one, and the
/// directives naming it, belong to the object format; the instruction sequence is the CPU's.
///
/// The slot is a hidden global, so its address is a link-time constant and the pair of `adrp` plus
/// `:lo12:` reaches it without a GOT.
pub fn indirect_jump_asm(slot: &str) -> String {
    format!("    adrp x16, {slot}\n    ldr x16, [x16, :lo12:{slot}]\n    br x16")
}

/// The single-byte counterpart is aarch64's four-byte `ret`, so what a consumer that only has a
/// byte can name is the low byte of that word.
pub const RET: u8 = 0xc0;

// ===== syscall interception trampoline =====
//
// Contract, which is the shape a real `svc` has on this platform:
// - In: x16 = call number, x0..x5 = the six arguments; out: x0 = return value.
// - A real syscall clobbers x0, x16, x17 and the flags and nothing else. `bl mirvm_syscall_dispatch`
//   is an ordinary call that would clobber far more, so the trampoline preserves the argument
//   registers and the condition flags around it, and restores x1 as well because this platform's
//   syscalls put a second result there.
// - Stack: the six argument slots double as the dispatch's argument array, and the trampoline's own
//   return address has to be saved because the `bl` overwrites the link register the rewritten site
//   left in it.
//
// Frame: 160 bytes = x0..x15 (128) + nzcv (8) + the return address (8), a multiple of 16 so the
// stack stays aligned for the call, which is the alignment both the ABI and `bl` require. x18 is
// reserved by this platform and x19 upwards are callee-saved, so neither side needs saving here.

unsafe extern "C" {
    fn mirvm_syscall_trampoline();
}

/// Get trampoline address, which would be filled into asm-stub/global-asm
pub fn syscall_trampoline_addr() -> u64 {
    mirvm_syscall_trampoline as *const () as u64
}

std::arch::global_asm!(
    ".globl mirvm_syscall_trampoline",
    ".p2align 4",
    "mirvm_syscall_trampoline:",
    "sub sp, sp, #160",
    "stp x0, x1, [sp, #0]",
    "stp x2, x3, [sp, #16]",
    "stp x4, x5, [sp, #32]",
    "stp x6, x7, [sp, #48]",
    "stp x8, x9, [sp, #64]",
    "stp x10, x11, [sp, #80]",
    "stp x12, x13, [sp, #96]",
    "stp x14, x15, [sp, #112]",
    "mrs x9, nzcv",
    "str x9, [sp, #128]",
    "str x30, [sp, #144]",
    // The dispatch takes (call number, pointer to the six argument slots).
    "mov x1, sp",
    "mov x0, x16",
    "bl mirvm_syscall_dispatch",
    // x0 is the call's result and is deliberately left alone from here on.
    "ldr x30, [sp, #144]",
    "ldr x9, [sp, #128]",
    "msr nzcv, x9",
    "ldp x10, x11, [sp, #80]",
    "ldp x12, x13, [sp, #96]",
    "ldp x14, x15, [sp, #112]",
    // x9 is restored last among these, so the flag value above survives until the write.
    "ldp x8, x9, [sp, #64]",
    "ldp x6, x7, [sp, #48]",
    "ldp x4, x5, [sp, #32]",
    "ldp x2, x3, [sp, #16]",
    "ldr x1, [sp, #8]",
    "add sp, sp, #160",
    "ret",
);

/// The contents of one inert symbol-image slot: `ret` followed by padding, so a slot start is
/// always a decodable instruction and nothing after the return is ever reached.
pub const INERT_SLOT: [u8; STUB_STRIDE as usize] = {
    let ret = 0xd65f_03c0_u32.to_le_bytes();
    let nop = 0xd503_201f_u32.to_le_bytes();
    [
        ret[0], ret[1], ret[2], ret[3], nop[0], nop[1], nop[2], nop[3], nop[0], nop[1], nop[2],
        nop[3], nop[0], nop[1], nop[2], nop[3],
    ]
};

/// The bridge entries for the calls `crate::vm::interpose` lists: this architecture's machine code,
/// reached by name because the platform's linker was asked to redirect those calls.
///
/// Each entry loads the owning engine into the argument register the replacement's trailing `owner`
/// parameter occupies -- the owner is always the last parameter, so its register is the count of the
/// call's own arguments -- and jumps indirect through the slot
/// `crate::vm::native_instance::wire` fills. A call's own arguments are never touched, and the
/// branch leaves the return address in `x30` in place, so the replacement returns to the caller.
///
/// The slots are hidden globals, so `adrp` plus `:lo12:` reaches each without a GOT. `x16` and `x17`
/// are the procedure call standard's intra-procedure scratch registers, which is why the entry may
/// use them without saving anything: they hold no argument and no result.
pub const NATIVE_RUNTIME_BRIDGE_ASM: &str = r#"
.text
.p2align 4
.globl __wrap_pthread_create
.hidden __wrap_pthread_create
.type __wrap_pthread_create,@function
__wrap_pthread_create:
    adrp x16, __mirvm_pthread_owner
    ldr x4, [x16, :lo12:__mirvm_pthread_owner]
    adrp x17, __mirvm_pthread_create_target
    ldr x17, [x17, :lo12:__mirvm_pthread_create_target]
    br x17
.size __wrap_pthread_create,.-__wrap_pthread_create

.p2align 4
.globl __wrap_pthread_key_create
.hidden __wrap_pthread_key_create
.type __wrap_pthread_key_create,@function
__wrap_pthread_key_create:
    adrp x16, __mirvm_pthread_owner
    ldr x2, [x16, :lo12:__mirvm_pthread_owner]
    adrp x17, __mirvm_pthread_key_create_target
    ldr x17, [x17, :lo12:__mirvm_pthread_key_create_target]
    br x17
.size __wrap_pthread_key_create,.-__wrap_pthread_key_create

.p2align 4
.globl __wrap_pthread_setspecific
.hidden __wrap_pthread_setspecific
.type __wrap_pthread_setspecific,@function
__wrap_pthread_setspecific:
    adrp x16, __mirvm_pthread_owner
    ldr x2, [x16, :lo12:__mirvm_pthread_owner]
    adrp x17, __mirvm_pthread_setspecific_target
    ldr x17, [x17, :lo12:__mirvm_pthread_setspecific_target]
    br x17
.size __wrap_pthread_setspecific,.-__wrap_pthread_setspecific

.p2align 4
.globl __wrap_pthread_key_delete
.hidden __wrap_pthread_key_delete
.type __wrap_pthread_key_delete,@function
__wrap_pthread_key_delete:
    adrp x16, __mirvm_pthread_owner
    ldr x1, [x16, :lo12:__mirvm_pthread_owner]
    adrp x17, __mirvm_pthread_key_delete_target
    ldr x17, [x17, :lo12:__mirvm_pthread_key_delete_target]
    br x17
.size __wrap_pthread_key_delete,.-__wrap_pthread_key_delete

.p2align 4
.globl __wrap_signal
.hidden __wrap_signal
.type __wrap_signal,@function
__wrap_signal:
    adrp x16, __mirvm_signal_owner
    ldr x2, [x16, :lo12:__mirvm_signal_owner]
    adrp x17, __mirvm_signal_target
    ldr x17, [x17, :lo12:__mirvm_signal_target]
    br x17
.size __wrap_signal,.-__wrap_signal

.p2align 4
.globl __wrap_sigaction
.hidden __wrap_sigaction
.type __wrap_sigaction,@function
__wrap_sigaction:
    adrp x16, __mirvm_signal_owner
    ldr x3, [x16, :lo12:__mirvm_signal_owner]
    adrp x17, __mirvm_sigaction_target
    ldr x17, [x17, :lo12:__mirvm_sigaction_target]
    br x17
.size __wrap_sigaction,.-__wrap_sigaction

.p2align 4
.globl __wrap_raise
.hidden __wrap_raise
.type __wrap_raise,@function
__wrap_raise:
    adrp x16, __mirvm_signal_owner
    ldr x1, [x16, :lo12:__mirvm_signal_owner]
    adrp x17, __mirvm_raise_target
    ldr x17, [x17, :lo12:__mirvm_raise_target]
    br x17
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

#[cfg(test)]
mod tests {
    use super::*;

    // The same bytes, written as assembly rather than as constants: the assembler is the authority
    // on the encoding, so the emitted patterns are compared against what it produces.
    std::arch::global_asm!(
        ".globl mirvm_test_plain_stub",
        ".p2align 4",
        "mirvm_test_plain_stub:",
        "ldr x16, #8",
        "br x16",
        ".quad 0x1122334455667788",
        ".globl mirvm_test_arg_stub",
        ".p2align 4",
        "mirvm_test_arg_stub:",
        "ldr x3, #12",
        "ldr x16, #16",
        "br x16",
        ".quad 0x99aabbccddeeff00",
        ".quad 0x0102030405060708",
        ".globl mirvm_test_ret",
        ".p2align 4",
        "mirvm_test_ret:",
        "ret",
        "nop",
        "nop",
        "nop",
    );

    unsafe extern "C" {
        static mirvm_test_plain_stub: u8;
        static mirvm_test_arg_stub: u8;
        static mirvm_test_ret: u8;
    }

    fn assembled(at: *const u8, len: usize) -> Vec<u8> {
        // SAFETY: each caller names a label the block above defines, with the length that label's
        // own byte pattern has.
        unsafe { std::slice::from_raw_parts(at, len) }.to_vec()
    }

    #[test]
    fn entry_stub_bytes_match_the_assembler() {
        let emitted = emit_stub_bytes(0x1122_3344_5566_7788);
        let expected = assembled(&raw const mirvm_test_plain_stub, emitted.len());
        assert_eq!(emitted.as_slice(), expected.as_slice());
        assert_eq!(emitted.len(), STUB_STRIDE as usize);
    }

    #[test]
    fn arg_stub_bytes_match_the_assembler() {
        let emitted = emit_arg_stub_bytes(0x99aa_bbcc_ddee_ff00, 0x0102_0304_0506_0708);
        let expected = assembled(&raw const mirvm_test_arg_stub, emitted.len());
        assert_eq!(emitted.as_slice(), expected.as_slice());
    }

    #[test]
    fn inert_slot_holds_a_return_and_padding() {
        let expected = assembled(&raw const mirvm_test_ret, INERT_SLOT.len());
        assert_eq!(INERT_SLOT.as_slice(), expected.as_slice());
        assert_eq!(INERT_SLOT[0], RET);
    }
}
