// Permanent negative-control probe: native code takes a fn-ptr out of a **guest struct field**
// and jumps to it, so the escape is embedded in the struct and never passes through an explicit
// fn-ptr argument slot (a shape the thunk mechanism cannot see). The cb value must be a stub
// code address landing in an executable entry; a guest frozen-region data address would send
// the native `jmp rax` into a read-write, non-executable region and fault silently (si_addr==rip).
use std::arch::global_asm;

// The "native side": real machine code (assembled by cc and materialized with dlopen, sharing no semantics with the guest).
// SysV: rdi = *const Holder; cb = [rdi+8], arg = [rdi]; tail call (the return value passes through rax).
global_asm!(
    ".globl mirvm_struct_cb_dispatch",
    ".type mirvm_struct_cb_dispatch, @function",
    "mirvm_struct_cb_dispatch:",
    "    mov rax, [rdi + 8]", // cb = holder->cb (fn-ptr embedded in the struct)
    "    mov rdi, [rdi]",     // arg = holder->value
    "    jmp rax",            // tail-call the embedded callback
);

#[repr(C)]
struct Holder {
    value: u64,
    cb: extern "C" fn(u64) -> u64,
}

extern "C" fn triple_plus_one(x: u64) -> u64 {
    x * 3 + 1
}

extern "C" fn xor_fold(x: u64) -> u64 {
    x ^ 0x5a5a
}

unsafe extern "C" {
    fn mirvm_struct_cb_dispatch(h: *const Holder) -> u64;
}

fn main() {
    let h1 = Holder {
        value: 20,
        cb: triple_plus_one,
    };
    let h2 = Holder {
        value: 0xff00,
        cb: xor_fold,
    };
    let r1 = unsafe { mirvm_struct_cb_dispatch(&h1) };
    let r2 = unsafe { mirvm_struct_cb_dispatch(&h2) };
    println!("struct-cb triple_plus_one(20) = {r1}");
    println!("struct-cb xor_fold(0xff00) = {r2}");
    assert_eq!(r1, 61);
    assert_eq!(r2, 0xa55a);
}
