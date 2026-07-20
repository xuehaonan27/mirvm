//! T1-b JIT 调用助手探针（m5.4-design §3.2）：
//! CallIndirect（dyn 虚派发 = fn_addrs 反查命中 / fn-ptr 直调）、
//! TlsRef（#[thread_local] 取址，mirvm_tls_ref 惰性物化同本体）、
//! InlineAsm（asm-stub 真地址直调，槽 ABI 同 interp）。
//! 三维（native / JIT-off / JIT=1）逐字节一致 + MIRVM_JIT_DEBUG 发布实证。
#![feature(thread_local)]

use std::arch::asm;

trait T {
    fn v(&self) -> u64;
}
struct A(u64);
impl T for A {
    fn v(&self) -> u64 {
        self.0 + 1
    }
}
struct B(u64);
impl T for B {
    fn v(&self) -> u64 {
        self.0 * 10
    }
}

#[thread_local]
static mut CNT: u64 = 0;

#[inline(never)]
fn call_dyn(x: &dyn T) -> u64 {
    x.v()
}

#[inline(never)]
fn apply(f: fn(u64) -> u64, x: u64) -> u64 {
    f(x) * 2
}

#[inline(never)]
fn plus3(x: u64) -> u64 {
    x + 3
}

#[inline(never)]
fn cpuid_leaf1() -> u64 {
    let out: u64;
    unsafe {
        asm!(
            "push rbx",
            "mov eax, 1",
            "cpuid",
            "mov {0}, rcx",
            "pop rbx",
            out(reg) out,
            out("rax") _,
        );
    }
    out
}

fn main() {
    let a = A(41);
    let b = B(5);
    let (mut da, mut db) = (0, 0);
    for _ in 0..30000 {
        da = call_dyn(&a);
        db = call_dyn(&b);
    }
    println!("dyn a={da} b={db}");
    let mut r = 0;
    for _ in 0..30000 {
        r = apply(plus3, 20);
    }
    println!("fnptr={r}");
    unsafe {
        *std::ptr::addr_of_mut!(CNT) += 40;
        *std::ptr::addr_of_mut!(CNT) += 2;
        println!("tls={}", std::ptr::read(&raw const CNT));
    }
    let mut c = 0;
    for _ in 0..30000 {
        c = cpuid_leaf1();
    }
    println!("cpuid ecx={c:#x}");
}
