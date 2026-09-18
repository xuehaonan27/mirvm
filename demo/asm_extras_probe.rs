// M5.2 D8g/D8h permanent differential probe: symbols defined by global_asm!, naked fn, inline asm const/sym
// operands, atexit LIFO callbacks — all compared against native on the same machine.
use std::arch::{asm, global_asm, naked_asm};
use std::os::raw::c_int;

global_asm!(
    ".globl mirvm_ax_triple",
    ".type mirvm_ax_triple, @function",
    "mirvm_ax_triple:",
    "    lea rax, [rdi + rdi*2]", // 3*x
    "    ret",
);

unsafe extern "C" {
    fn mirvm_ax_triple(x: u64) -> u64;
}

#[unsafe(naked)]
extern "C" fn naked_sub(a: u64, b: u64) -> u64 {
    naked_asm!("mov rax, rdi", "sub rax, rsi", "ret")
}

extern "C" fn bye_a() {
    println!("bye A");
}
extern "C" fn bye_b() {
    println!("bye B");
}
unsafe extern "C" {
    fn atexit(f: extern "C" fn()) -> c_int;
}

fn inline_const(v: u64) -> u64 {
    let mut r = v;
    unsafe {
        asm!("shl {0}, {s}", inout(reg) r, s = const 3u32);
    }
    r
}

fn main() {
    println!("global_asm triple(14) = {}", unsafe { mirvm_ax_triple(14) });
    println!("naked_sub(50, 8) = {}", naked_sub(50, 8));
    println!("inline const <<3 (5) = {}", inline_const(5));
    unsafe {
        atexit(bye_a);
        atexit(bye_b);
    }
    println!("main done");
}
