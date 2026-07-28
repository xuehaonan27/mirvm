// D15 P2 切② 对拍夹具（tests/diff_cless.sh）：path proc-macro 项目形态。
// 输出确定性文本——cargo 腿（MIRVM_DEPS=cargo + MIRVM_CARGO_LOCKED=1）与
// self 腿（MIRVM_DEPS=self：proc-macro 真 rustc host 编译）逐字节对拍。
use my_derive::Hello;

#[derive(Hello)]
struct S;

fn main() {
    println!("cless-pm derive={}", S::hello());
    std::process::exit(5);
}
