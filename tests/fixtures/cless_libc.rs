---
[dependencies]
libc = "0.2"
---

// differential.cargoless 的 registry build.rs 全生命周期夹具。
// （libc 自带 build.rs——host 编译 → 执行 → rustc-cfg 进本 crate 编译）。
// 输出确定性文本——cargo 腿（MIRVM_DEPS=cargo）与 self 腿（MIRVM_DEPS=self）
// 逐字节对拍。
fn main() {
    println!(
        "cless-libc stdout={} stderr={} einval={}",
        libc::STDOUT_FILENO,
        libc::STDERR_FILENO,
        libc::EINVAL
    );
    std::process::exit(6);
}
