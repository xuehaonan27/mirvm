#![feature(linkage)]
// E27：extern weak 符号定向验收（地址模型 P2 GOT 启动相重填，open-issues E27 关账探针）。
// 使用 rustc 唯一官方形态：「Option<extern fn> 类型的 #[linkage="extern_weak"] static」
// （cg_llvm consts.rs check_and_apply_linkage——真符号按签名单 extern_weak 化，链接期
// 无定义则内部格初始化为 0）。两路对拍 native：
// ① 无定义 weak 符号 → static 值 = None（ELF 弱符号缺席语义）；不调用（native 调 = UB）。
// ② weak 声明 + 强定义（libc getpid）→ Some，可调用；pid 进程相关，只验同进程
//    两次一致且为正，不比具体值。
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
