//! 真实 `lang_start` 主程序结果分类探针。

use std::process::{ExitCode, Termination};

struct Exit101;

impl Termination for Exit101 {
    fn report(self) -> ExitCode {
        ExitCode::from(101)
    }
}

fn main() -> Exit101 {
    if std::env::args().any(|arg| arg == "panic") {
        panic!("main outcome probe");
    }
    Exit101
}
