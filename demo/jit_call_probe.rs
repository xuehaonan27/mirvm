//! T1-b JIT call-helper probe (m5.4-design §3.2):
//! CallIndirect (dyn virtual dispatch = fn_addrs reverse lookup hit / fn-ptr direct call),
//! TlsRef (#[thread_local] address-of, mirvm_tls_ref lazily materialized same as host),
//! InlineAsm (asm-stub real-address direct call, slot ABI same as interp).
//! Byte-identical across three dimensions (native / JIT-off / JIT=1) + MIRVM_JIT_DEBUG release evidence.
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
