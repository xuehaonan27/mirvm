---
[dependencies]
libc = "0.2"
---

// differential.cargoless registry build.rs full-lifecycle fixture.
// (libc ships its own build.rs — host compile → run → rustc-cfg into this crate's compile).
// Deterministic output — byte-identical between cargo leg (MIRVM_DEPS=cargo) and self leg (MIRVM_DEPS=self).
fn main() {
    println!(
        "cless-libc stdout={} stderr={} einval={}",
        libc::STDOUT_FILENO,
        libc::STDERR_FILENO,
        libc::EINVAL
    );
    std::process::exit(6);
}
