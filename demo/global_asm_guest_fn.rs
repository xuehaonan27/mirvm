use std::arch::global_asm;

// global_asm `sym` pointing at an interpreted guest fn: the machine code (probe_native_chain)
// calls the guest fn via `call {guest}`. On the mirvm side the symbol is pre-allocated as an
// ABS definition through the P1 entry stub, so the call lands there and trampolines back to
// the interpreter; natively it is a plain direct call. Return value + pointer write-back must match.
#[unsafe(no_mangle)]
pub extern "C" fn probe_guest(x: u64, slot: *mut u64) -> u64 {
    unsafe {
        *slot = *slot + 1;
    }
    x * 3 + 1
}

global_asm!(
    r#"
.globl probe_native_chain
.type probe_native_chain,@function
probe_native_chain:
    push rbx
    mov rbx, rdi
    mov rdi, rbx
    call {guest}
    add rbx, rax
    mov rax, rbx
    pop rbx
    ret
"#,
    guest = sym probe_guest
);

unsafe extern "C" {
    fn probe_native_chain(x: u64, slot: *mut u64) -> u64;
}

fn main() {
    let mut slot: u64 = 40;
    let y = unsafe { probe_native_chain(7, &mut slot) };
    // Expected: guest(7, &slot) leaves slot 41 and returns 22; chain = 7 + 22 = 29
    println!("chain={y} slot={slot}");
}
