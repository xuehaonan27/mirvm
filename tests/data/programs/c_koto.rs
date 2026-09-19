#!/usr/bin/env mirvm
---
[dependencies]
# koto =0.15.3: the newest on the 0.15 line (0.16.x is out of scope for the
# newest-0.15 pinning rule). features = default (default = ["rc"] -- Rc memory
# management; the arc/rc memory strategies conflict, so the crate default stands).
# The closure is about 30 crates: koto_bytecode/koto_parser/koto_runtime/koto_lexer/
# koto_memory/koto_derive (proc-macro, compile-time only)/indexmap/rustc-hash/smallvec/
# saturating_cast/downcast-rs/thiserror/chrono/hashbrown and so on, with no C or SIMD.
koto = "=0.15.3"
---
// koto 0.15.3 script language (a VM inside the VM: bytecode compilation + tree-walking
// VM) three-way differential driver.
//
// Coverage:
//   1) Script A, the map/filter pipeline: the each/keep/sum/fold/zip/count/min_max/
//      to_list/to_tuple iterator adapters and consumers, plus the host-injected fbits
//      float stub.
//   2) Script B, closures and capture: koto captures by value at creation time (rebinding
//      an outer variable does not affect the closure), mutable state through a captured
//      container (state.count), a closure factory (make_adder), recursive closure fib, and
//      callbacks into host functions (host_mul_add / bump / mark).
//   3) Script C, for/while loops: for over a range with continue, for over a list, while
//      with a break value, until, loop with a break value, nested for, and host callback
//      side effects (bump) inside loops.
//   Host injection: prelude values (host_seed/host_scale/host_name) and four callbacks
//   (bump, mark, host_mul_add, fbits). Output = in-script print plus host-side
//   value_to_string of each return value plus the side-effect counter and log.
//
// Determinism: koto's ValueMap is IndexMap + FxHasher (fixed seed, not RandomState), so map
// traversal is insertion order and cross-process deterministic; floats always print to_bits()
// via fbits; no clock/os/io input is called; each script gets its own Koto instance. stderr
// is empty because the dependency emits no compile warnings and native uses cargo run -q.
//
// Three-way rerun:
//   A: target/release/mirvm run tests/data/programs/c_koto.rs
//   B: cd "$(grep -l 'name = "c_koto"' ~/.cache/mirvm/scripts/*/Cargo.toml \
//        | xargs dirname)" && RUSTC=~/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/rustc \
//        ~/.rustup/toolchains/nightly-2026-07-02-x86_64-unknown-linux-gnu/bin/cargo run -q
//   C: MIRVM_JIT_THRESHOLD=1 target/release/mirvm run tests/data/programs/c_koto.rs
use koto::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

type Counter = Rc<RefCell<i64>>;
type Log = Rc<RefCell<Vec<String>>>;

/// Register host injections (values + callbacks) on a fresh Koto instance and run the
/// script; returns the script result for the caller to print.
fn run_script(
    label: &str,
    script: &str,
    bump_count: &Counter,
    marks: &Log,
) -> Result<String, String> {
    let mut koto = Koto::new();
    let prelude = koto.prelude();

    // ---- host-injected values ----
    prelude.insert("host_seed", 7);
    prelude.insert("host_scale", 2.5);
    prelude.insert("host_name", "koto-host");

    // ---- host-injected callbacks ----
    // Side-effect counter: bump(n) accumulates and returns the new total.
    {
        let bump_count = Rc::clone(bump_count);
        prelude.add_fn("bump", move |ctx| match ctx.args() {
            [KValue::Number(KNumber::I64(n))] => {
                let mut g = bump_count.borrow_mut();
                *g += n;
                Ok(KValue::Number(KNumber::I64(*g)))
            }
            unexpected => unexpected_args("|Number|", unexpected),
        });
    }
    // Logging: mark(tag) records the tag and returns the current count.
    {
        let marks = Rc::clone(marks);
        prelude.add_fn("mark", move |ctx| match ctx.args() {
            [KValue::Str(tag)] => {
                let mut g = marks.borrow_mut();
                g.push(tag.to_string());
                Ok(KValue::Number(KNumber::I64(g.len() as i64)))
            }
            unexpected => unexpected_args("|String|", unexpected),
        });
    }
    // Pure function: host_mul_add(a, b, c) = a * b + c.
    prelude.add_fn("host_mul_add", |ctx| match ctx.args() {
        [KValue::Number(a), KValue::Number(b), KValue::Number(c)] => Ok((*a * *b + *c).into()),
        unexpected => unexpected_args("|Number, Number, Number|", unexpected),
    });
    // Float bit lock: fbits(x) is the hex string of f64.to_bits().
    prelude.add_fn("fbits", |ctx| match ctx.args() {
        [KValue::Number(KNumber::F64(x))] => Ok(format!("0x{:016x}", x.to_bits()).into()),
        unexpected => unexpected_args("|Float|", unexpected),
    });

    match koto.compile_and_run(script) {
        Ok(result) => koto
            .value_to_string(result)
            .map_err(|e| format!("{label}: display failed: {e}")),
        Err(e) => Err(format!("{label}: {e}")),
    }
}

// ---- Script A: the map/filter iterator pipeline ----
const SCRIPT_A: &str = r#"
pipe1 = [1, 2, 3, 4, 5, 6, 7, 8, 9]
  .each |n| n * n
  .keep |n| n % 2 == 0
  .to_list()
print 'pipe1: {pipe1}'

pipe2 = [1, 2, 3, 4, 5, 6, 7, 8, 9]
  .keep |n| n % 2 == 1
  .each |n| n + host_seed
  .sum()
print 'pipe2 sum: {pipe2}'

folded = [10, 20, 30, 40]
  .fold 'acc', |acc, n| '{acc}/{n}'
print 'fold: {folded}'

pairs = (1, 2, 3)
  .zip ('a', 'b', 'c')
  .to_list()
print 'zip: {pairs}'

evens = (3, 8, 5, 12, 7, 22)
  .keep |n| n % 2 == 0
  .to_tuple()
print 'evens: {evens} count: {size(evens)}'

mm = (-3, 0, 7, 2, 9).min_max()
print 'min_max: {mm}'

big = (10, 25, 41, 9, 33)
  .keep |n| n % 2 == 1
  .count()
print 'odd count: {big}'

print 'host_name: {host_name}'
print 'float bits: {fbits(host_scale * 3.0 + 0.5)}'
print 'neg float bits: {fbits(-1.5 / 7.0)}'

host_seed * 2
"#;

// ---- Script B: closures and capture (koto captures by value at creation) ----
const SCRIPT_B: &str = r#"
x = 5
f = |n| n + x
x = 100
print 'capture-copy: {f(2)}'

state = {count: 0}
inc = |n|
  state.count += n
  state.count
print 'container-capture: {inc(5)}, {inc(7)}, {inc(-2)}, {state.count}'

make_adder = |n| |m| m + n
add3 = make_adder(3)
add10 = make_adder(10)
print 'adder: {add3(4)}, {add10(4)}'

fib = |n| if n < 2 then n else (fib n - 1) + (fib n - 2)
print 'fib(10): {fib(10)}'

print 'host_mul_add: {host_mul_add(6, 7, 1)}'
apply = |g, a, b|
  g(a, b) + 1
plus = |a, b| a + b
print 'higher-order: {apply(plus, 20, 21)}'

b1 = bump(3)
b2 = bump(4)
print 'bump: {b1}, {b2}'

m1 = mark('be')
m2 = mark('am')
print 'mark: {m1}, {m2}'

'closures done'
"#;

// ---- Script C: for/while loops ----
const SCRIPT_C: &str = r#"
total = 0
for i in 1..=15
  if i % 3 == 0
    continue
  total += i
print 'for-sum-skip3: {total}'

words = ['a', 'bb', 'ccc', 'dddd']
longest = ''
for w in words
  if size(w) > size(longest)
    longest = w
print 'longest: {longest}'

n = 2
found = while n < 500
  if n * n * n > 300
    break n
  n += 1
print 'while-break: {found}'

stack = [1, 2, 3]
popped = []
until stack.is_empty()
  popped.push(stack.pop())
print 'until-pop: {popped}'

acc = 1
rounds = loop
  acc *= 2
  if acc > 20
    break acc
print 'loop-break: {rounds}'

pairs = []
for i in 1..=3
  for j in (10, 20)
    pairs.push('{i}:{j}')
print 'nested: {pairs}'

for i in 1..=4
  bump(i)

'loops done'
"#;

fn main() {
    let bump_count: Counter = Rc::new(RefCell::new(0));
    let marks: Log = Rc::new(RefCell::new(Vec::new()));

    for (label, script) in [
        ("script A", SCRIPT_A),
        ("script B", SCRIPT_B),
        ("script C", SCRIPT_C),
    ] {
        match run_script(label, script, &bump_count, &marks) {
            Ok(result) => println!("{label} result = {result}"),
            Err(e) => println!("{label} error = {e}"),
        }
    }

    println!("bump counter = {}", *bump_count.borrow());
    println!("marks = {:?}", *marks.borrow());
}
