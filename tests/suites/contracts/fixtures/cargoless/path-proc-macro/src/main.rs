// differential.cargoless path proc-macro project fixture.
// It emits deterministic text so the cargo leg (MIRVM_DEPS=cargo +
// MIRVM_CARGO_LOCKED=1) and the self leg (MIRVM_DEPS=self, proc-macro host-compiled by real rustc) compare byte-for-byte.
use my_derive::Hello;

#[derive(Hello)]
struct S;

fn main() {
    println!("cless-pm derive={}", S::hello());
    std::process::exit(5);
}
