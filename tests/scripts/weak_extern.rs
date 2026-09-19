#![feature(linkage)]
// extern weak symbol checks: rustc's only official form is a `#[linkage="extern_weak"] static`
// of type `Option<extern fn>` (cg_llvm consts.rs check_and_apply_linkage turns a real symbol into a single
// extern_weak per signature; an undefined symbol at link time leaves the internal slot at 0). Compared with native:
// 1. Undefined weak symbol -> the static value is None (ELF weak-symbol-absent semantics); it is not called,
//    since calling it natively is UB.
// 2. Weak declaration + strong definition (libc getpid) -> Some and callable; the pid is process-dependent,
//    so only same-process consistency and positivity are checked, not the concrete value.
unsafe extern "C" {
    #[linkage = "extern_weak"]
    static MIRVM_ABSENT_PROBE_SYMBOL_XYZ: Option<unsafe extern "C" fn() -> i32>;
    #[linkage = "extern_weak"]
    static getpid: Option<unsafe extern "C" fn() -> i32>;
}

fn main() {
    let absent = unsafe { MIRVM_ABSENT_PROBE_SYMBOL_XYZ };
    println!("absent-none={}", absent.is_none());
    let hit = unsafe { getpid };
    println!("hit-some={}", hit.is_some());
    match hit {
        None => println!("hit-call=skipped-impossible"),
        Some(f) => {
            let (a, b) = unsafe { (f(), f()) };
            println!("hit-call-consistent={}", a == b && a > 0);
        }
    }
}
