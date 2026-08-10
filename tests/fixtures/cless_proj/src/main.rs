// differential.cargoless 的 Cargo 项目夹具（带 Cargo.lock
// 锁版）。输出确定性文本——cargo 腿（MIRVM_DEPS=cargo + MIRVM_CARGO_LOCKED=1）
// 与 self 腿（MIRVM_DEPS=self）逐字节对拍。
use cfg_if::cfg_if;

fn main() {
    let mut buf = itoa::Buffer::new();
    cfg_if! {
        if #[cfg(unix)] {
            let msg = "unix";
        } else {
            let msg = "other";
        }
    }
    println!("cless-proj itoa={} cfg={msg}", buf.format(67890));
    std::process::exit(3);
}
