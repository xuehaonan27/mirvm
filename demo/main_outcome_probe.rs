//! Real `lang_start` main-program outcome classification probe.

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
