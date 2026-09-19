#!/usr/bin/env mirvm
---
[dependencies]
rustpython-vm = "=0.5.0"
# Upstream ABI break: from libc 0.2.189 on, Linux's POSIX_SPAWN_SETSID is a narrower type
# than c_int, so rustpython-vm 0.5.0's posix.rs call to PosixSpawnFlags::from_bits_retain
# (an i32 parameter) fails to compile with E0308. A plain cargo resolve picks 0.2.189 and
# breaks the same way, so this is not a mirvm fork. Pinned to =0.2.186, the version in the
# lockfile from when the driver was accepted and verified to compile.
libc = "=0.2.186"
---
// c_rustpython_mini -- probing a large dependency: an embedded bare interpreter
// (without_stdlib) runs five fixed Python programs; each result is recovered from the
// globals as a str named RESULT and printed by the host.
//
// Coverage:
//   P1 arith    : integer arithmetic (* + // % **, floor semantics for negatives, abs)
//   P2 listdict : dict insert/overwrite, sorted ordering, list comprehension, str.join / repr
//   P3 closure  : factory function + closure cell state, lambda, map-style apply, list slicing
//   P4 exc      : try/except catching ZeroDivisionError / raising ValueError / KeyError,
//                 printing type(e).__name__ and the str(e)/repr(str(e)) exception text
//   P5 strfmt   : strip/upper/split, dict comprehension, string reversal, f-string concatenation
//
// The oracle is stdout: the five "P<n>=..." lines must be byte-identical between the native
// build and mirvm, with stderr empty and exit 0. run_one compiles with
// vm.compile(src, Mode::Exec, "<tag>"), runs the code object in a Scope::with_builtins dict
// and reads RESULT back; every failure maps to a fixed sentinel
// (COMPILE_ERR/UNCAUGHT/MISSING/NOT_STR), so a broken program cannot pass silently.
//
// Determinism:
//   - Every program uses only integers and ASCII strings; no floats, no import (the bare vm
//     has no importlib or stdlib).
//   - No real randomness, wall clock or threads; any dict-order output goes through an
//     explicit sorted() first.
//   - On the Python side str()/repr() only see int/str scalars and containers, never a
//     function or object, so no raw addresses appear.
//   - The host copies RESULT's utf8 text to stdout byte for byte; the exit code is always 0.
//
// Version pin: rustpython-vm "=0.5.0" (latest stable on crates.io, released 2026-03-31).
// The lock tree has 230 packages; a cold debug build takes 1m08s on this 8-core machine,
// well inside the 15-minute budget. libffi-sys 4.2.0 runs vendored through cc and psm
// 0.1.31 uses built-in assembly through cc; both pass, and no bindgen/nasm/clang is needed.
//
// Official-switch workaround: Settings.install_signal_handlers = false (rustpython-vm's
// public embedding API, src/vm/setting.rs:50, default true). Why it is needed:
//   ① The first A-dimension run exits 70 with stderr "guest handler for synchronous fault
//      signal 7" -- mirvm loudly refusing a guest handler for a synchronous fault signal.
//   ② With install_signal_handlers = true, rustpython-vm's `_signal` builtin module init
//      (stdlib/_signal.rs init_signal_handlers) queries and re-installs every signal in
//      1..NSIG through libc::signal(n, SIG_IGN) and libc::signal(n, handler).
//   ③ The "original handler" for SIGBUS(7) is the stack_overflow::imp::signal_handler that
//      the guest-side Rust std runtime installs at startup; it is a guest fn, and reaching
//      it through mirvm's thunk lands in the synchronous-fault refusal, hence exit 70.
//   ④ That installation only serves the CLI REPL's SIGINT/default_int_handler takeover and
//      the signal module's handler registry. This driver uses no signal module and has no
//      REPL, so turning it off changes nothing about the Python semantics under test (a
//      native rerun with the same setting gives byte-identical output for all five).
//
// NOTE: a panic on the mirvm-jit worker thread does not change stdout or the exit code,
// because the JIT keeps interpreting when a worker dies; the frame analysis that can panic
// on a bad address-taking offset is in the JIT's frame layer, so a JIT-dimension guard must
// inspect stderr rather than the exit code.
//
// Each program runs in its own fresh globals dict inside the single Interpreter created at
// startup (Settings with install_signal_handlers = false); no files are written and the
// only environment input is MIRVM_JIT_THRESHOLD.
//
// Output is exactly five lines, one per program, in P1..P5 order, each of the form
// "P<n>=<RESULT text>". Nothing else is printed, so a missing line is itself a failure
// signal, and the five tags make a regression localizable to one program.
// that broke (compile, run, RESULT lookup or type).
//
// The Python sources are embedded as raw string literals and are part of the test data.
use rustpython_vm::builtins::PyStr;
use rustpython_vm::compiler::Mode;
use rustpython_vm::scope::Scope;
use rustpython_vm::{Interpreter, Settings, VirtualMachine};

const PROGRAMS: &[(&str, &str)] = &[
    (
        "P1_arith",
        r#"
a = 2 + 3 * 4
b = (a * 10) // 7
c = (2 ** 10) % 97
d = (-15) // 4
e = (-15) % 4
f = abs(-9) * (17 // -3)
RESULT = repr([a, b, c, d, e, f])
"#,
    ),
    (
        "P2_listdict",
        r#"
d = {}
for k in ["delta", "alpha", "charlie", "bravo"]:
    d[k] = len(k) * 7
keys = sorted(d.keys())
pairs = [k + ":" + str(d[k]) for k in keys]
nums = [x * x for x in range(12) if x % 3 == 0]
d["alpha"] += 100
RESULT = ",".join(pairs) + "|" + repr(nums) + "|" + repr(d["alpha"])
"#,
    ),
    (
        "P3_closure",
        r#"
def make_counter(step):
    total = [0]
    def bump():
        total[0] += step
        return total[0]
    return bump

c1 = make_counter(3)
c2 = make_counter(10)
vals = [c1(), c1(), c2(), c1(), c2()]

def apply(f, xs):
    return [f(x) for x in xs]

sq = apply(lambda x: x * x, vals[:3])
RESULT = "->".join(str(v) for v in vals) + "//" + ",".join(str(x) for x in sq)
"#,
    ),
    (
        "P4_exc",
        r#"
out = []

def explode(x):
    return 100 // x

try:
    explode(2)
    explode(0)
    out.append("noexc")
except ZeroDivisionError as e:
    out.append("caught:" + type(e).__name__ + ":" + str(e))

try:
    raise ValueError("explicit-" + str(6 * 7))
except ValueError as e:
    out.append(str(e).upper())

xs = {"k": 1}
try:
    xs["missing"]
except BaseException as e:
    out.append(type(e).__name__ + ":" + repr(str(e)))

RESULT = "|".join(out)
"#,
    ),
    (
        "P5_strfmt",
        r#"
s = " hello,mirvm "
parts = (s.strip() + " X7").upper().split(" ")
nums = [str(len(p)) for p in parts]
tbl = {p: p[::-1] for p in parts if p}
ks = sorted(tbl)
RESULT = f"{len(s)}:{'-'.join(nums)}:" + "|".join(f"{k}={tbl[k]}" for k in ks)
"#,
    ),
];

fn run_one(vm: &VirtualMachine, tag: &str, src: &str) {
    let globals = vm.ctx.new_dict();
    let scope = Scope::with_builtins(None, globals.clone(), vm);
    let captured = match vm.compile(src, Mode::Exec, format!("<{tag}>")) {
        Err(_) => "COMPILE_ERR".to_owned(),
        Ok(code) => match vm.run_code_obj(code, scope) {
            Err(_) => "UNCAUGHT".to_owned(),
            Ok(_) => match globals.get_item("RESULT", vm) {
                Err(_) => "MISSING".to_owned(),
                Ok(obj) => match obj.downcast::<PyStr>() {
                    Err(_) => "NOT_STR".to_owned(),
                    Ok(s) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
                },
            },
        },
    };
    println!("{tag}={captured}");
}

fn main() {
    let mut settings = Settings::default();
    settings.install_signal_handlers = false;
    let interp = Interpreter::without_stdlib(settings);
    interp.enter(|vm| {
        for (tag, src) in PROGRAMS {
            run_one(vm, tag, src);
        }
    });
}
