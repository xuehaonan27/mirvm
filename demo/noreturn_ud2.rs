// C3 spike 面孔②：ud2 终止形——flushed 打印后调 ud2 noreturn stub，
// 宿主收 SIGILL（native 同；两侧 stderr 真空、exit=128+SIGILL）。
use std::io::Write as _;

fn main() {
    println!("before-ud2");
    std::io::stdout().flush().unwrap();
    unsafe {
        std::arch::asm!("ud2", options(noreturn));
    }
}
