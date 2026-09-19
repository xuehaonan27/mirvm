// differential.cargoless Cargo project fixture (with a version-pinning
// Cargo.lock). It emits deterministic text so the cargo leg (MIRVM_DEPS=cargo +
// MIRVM_CARGO_LOCKED=1) and the self leg (MIRVM_DEPS=self) compare byte-for-byte.
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
