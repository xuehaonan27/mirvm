// C3 spike face ②: ud2 termination form — after flushed print, call ud2 noreturn stub,
// host receives SIGILL (same as native; both sides have empty stderr, exit=128+SIGILL).
use std::io::Write as _;

fn main() {
    println!("before-ud2");
    std::io::stdout().flush().unwrap();
    unsafe {
        std::arch::asm!("ud2", options(noreturn));
    }
}
