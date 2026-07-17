// P1 永久负对照探针（debt-map §6 的缩微模型 / decision-history §7.6）：
// native 代码从 **guest 结构体字段**里取 fn-ptr 并跳转——结构体内嵌逃逸，
// 不走任何显式 fn-ptr 实参位（thunk 机制看不见的逃逸姿势）。
// P1 前：cb 值 = guest 冻结区数据地址，native `jmp rax` 跳进 rw 非可执行域，
// 静默 SIGSEGV（si_addr==rip）；P1 后：cb 值 = stub 码址，直落可执行入口。
use std::arch::global_asm;

// "native 侧"：真机器码（cc 汇编 + dlopen 物化，与 guest 无 shared 语义）。
// SysV：rdi = *const Holder；cb = [rdi+8]，arg = [rdi]，尾调（返回值透传 rax）。
global_asm!(
    ".globl mirvm_struct_cb_dispatch",
    ".type mirvm_struct_cb_dispatch, @function",
    "mirvm_struct_cb_dispatch:",
    "    mov rax, [rdi + 8]", // cb = holder->cb（结构体内嵌 fn-ptr）
    "    mov rdi, [rdi]",     // arg = holder->value
    "    jmp rax",            // 尾调内嵌回调
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
