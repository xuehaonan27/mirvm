//! Spike 1 differential harness: three hand-written bytecode programs compared in-process
//! with native Rust references.
//!
//! Each program pins one core mechanism:
//! - fib (recursive): host-recursive Call + SwitchInt + BinOp (model A core)
//! - loop-sum       : Goto/SwitchInt loop (non-recursive control flow)
//! - mem-array-sum  : real-address bare memory (Alloc/Store/Load)

use std::process::ExitCode;

use super::bytecode::{
    BinOp, Block, Body, Operand, Program, Rvalue, Stmt, Terminator, UnwindAction,
};
use super::interp::Vm;

// Shorthand constructors to keep hand-written bytecode readable.
fn s(n: u32) -> Operand {
    Operand::Slot(n)
}
fn k(n: u64) -> Operand {
    Operand::Const(n)
}
fn asgn(dst: u32, rv: Rvalue) -> Stmt {
    Stmt::Assign(dst, rv)
}
fn bin(op: BinOp, a: Operand, b: Operand) -> Rvalue {
    Rvalue::Binary(op, a, b)
}

/// fib(n) = n<2 ? n : fib(n-1)+fib(n-2)
/// Slots: 0=ret 1=n 2=cond 3=n-1 4=fib(n-1) 5=n-2 6=fib(n-2)
fn build_fib() -> Program {
    use BinOp::*;
    use Rvalue::Use;
    use Terminator::*;
    let blocks = vec![
        // bb0: _2 = n < 2; switch(_2){0=>rec} else base
        Block {
            stmts: vec![asgn(2, bin(Lt, s(1), k(2)))],
            term: SwitchInt {
                discr: s(2),
                targets: vec![(0, 2)],
                otherwise: 1,
            },
        },
        // bb1 (base, n<2): _0 = n; return
        Block {
            stmts: vec![asgn(0, Use(s(1)))],
            term: Return,
        },
        // bb2 (rec): _3 = n-1; _4 = fib(_3) -> bb3
        Block {
            stmts: vec![asgn(3, bin(Sub, s(1), k(1)))],
            term: Call {
                func: 0,
                args: vec![s(3)],
                dst: 4,
                target: 3,
                unwind: UnwindAction::Continue,
            },
        },
        // bb3: _5 = n-2; _6 = fib(_5) -> bb4
        Block {
            stmts: vec![asgn(5, bin(Sub, s(1), k(2)))],
            term: Call {
                func: 0,
                args: vec![s(5)],
                dst: 6,
                target: 4,
                unwind: UnwindAction::Continue,
            },
        },
        // bb4: _0 = _4 + _6; return
        Block {
            stmts: vec![asgn(0, bin(Add, s(4), s(6)))],
            term: Return,
        },
    ];
    Program {
        funcs: vec![Body {
            num_slots: 7,
            num_args: 1,
            blocks,
        }],
    }
}

/// sum(n) = 1+2+...+n (iterative). Slots: 0=acc/ret 1=n 2=i 3=cond
fn build_loop_sum() -> Program {
    use BinOp::*;
    use Rvalue::Use;
    use Terminator::*;
    let blocks = vec![
        // bb0: acc=0; i=1; goto head
        Block {
            stmts: vec![asgn(0, Use(k(0))), asgn(2, Use(k(1)))],
            term: Goto(1),
        },
        // bb1 (head): _3 = i<=n; switch(_3){0=>exit} else body
        Block {
            stmts: vec![asgn(3, bin(Le, s(2), s(1)))],
            term: SwitchInt {
                discr: s(3),
                targets: vec![(0, 3)],
                otherwise: 2,
            },
        },
        // bb2 (body): acc+=i; i+=1; goto head
        Block {
            stmts: vec![asgn(0, bin(Add, s(0), s(2))), asgn(2, bin(Add, s(2), k(1)))],
            term: Goto(1),
        },
        // bb3 (exit): return
        Block {
            stmts: vec![],
            term: Return,
        },
    ];
    Program {
        funcs: vec![Body {
            num_slots: 4,
            num_args: 1,
            blocks,
        }],
    }
}

/// memsum(n) = 0+1+...+(n-1) through real-address memory: alloc n*8, store i, then load
/// and sum.
/// Slots: 0=acc/ret 1=n 2=base 3=i 4=cond 5=addr 6=tmp
fn build_mem_sum() -> Program {
    use BinOp::*;
    use Rvalue::{Alloc, Load, Use};
    use Terminator::*;
    let blocks = vec![
        // bb0: _6=n*8; base=alloc(_6); i=0; goto store-head
        Block {
            stmts: vec![
                asgn(6, bin(Mul, s(1), k(8))),
                asgn(2, Alloc(s(6))),
                asgn(3, Use(k(0))),
            ],
            term: Goto(1),
        },
        // bb1 (store-head): _4 = i<n; switch{0=>sum-init} else store-body
        Block {
            stmts: vec![asgn(4, bin(Lt, s(3), s(1)))],
            term: SwitchInt {
                discr: s(4),
                targets: vec![(0, 3)],
                otherwise: 2,
            },
        },
        // bb2 (store-body): addr=base+i*8; *addr=i; i+=1; goto store-head
        Block {
            stmts: vec![
                asgn(5, bin(Mul, s(3), k(8))),
                asgn(5, bin(Add, s(2), s(5))),
                Stmt::Store(s(5), s(3)),
                asgn(3, bin(Add, s(3), k(1))),
            ],
            term: Goto(1),
        },
        // bb3 (sum-init): acc=0; i=0; goto sum-head
        Block {
            stmts: vec![asgn(0, Use(k(0))), asgn(3, Use(k(0)))],
            term: Goto(4),
        },
        // bb4 (sum-head): _4 = i<n; switch{0=>exit} else sum-body
        Block {
            stmts: vec![asgn(4, bin(Lt, s(3), s(1)))],
            term: SwitchInt {
                discr: s(4),
                targets: vec![(0, 6)],
                otherwise: 5,
            },
        },
        // bb5 (sum-body): addr=base+i*8; tmp=*addr; acc+=tmp; i+=1; goto sum-head
        Block {
            stmts: vec![
                asgn(5, bin(Mul, s(3), k(8))),
                asgn(5, bin(Add, s(2), s(5))),
                asgn(6, Load(s(5))),
                asgn(0, bin(Add, s(0), s(6))),
                asgn(3, bin(Add, s(3), k(1))),
            ],
            term: Goto(4),
        },
        // bb6 (exit): return
        Block {
            stmts: vec![],
            term: Return,
        },
    ];
    Program {
        funcs: vec![Body {
            num_slots: 7,
            num_args: 1,
            blocks,
        }],
    }
}

// ---- native reference implementations ----
fn fib_ref(n: u64) -> u64 {
    if n < 2 {
        n
    } else {
        fib_ref(n - 1) + fib_ref(n - 2)
    }
}
fn loop_sum_ref(n: u64) -> u64 {
    (1..=n).sum()
}
fn mem_sum_ref(n: u64) -> u64 {
    (0..n).sum()
}

/// Run a program over a set of inputs; PASS only if all match the reference.
fn check(name: &str, prog: &Program, inputs: &[u64], reference: impl Fn(u64) -> u64) -> bool {
    for &n in inputs {
        let mut vm = Vm::new(prog);
        let got = vm.run(0, &[n]);
        let want = reference(n);
        if got != want {
            println!("FAIL {name}({n}): got {got}, want {want}");
            return false;
        }
    }
    println!("PASS {name} ({} inputs)", inputs.len());
    true
}

pub fn run() -> ExitCode {
    let fib = build_fib();
    let sum = build_loop_sum();
    let mem = build_mem_sum();

    let n_fib: Vec<u64> = (0..=35).collect();
    let n_sum: Vec<u64> = (0..=1000).collect();
    let n_mem: Vec<u64> = (0..=256).collect();

    let mut ok = true;
    ok &= check("fib", &fib, &n_fib, fib_ref);
    ok &= check("loop_sum", &sum, &n_sum, loop_sum_ref);
    ok &= check("mem_sum", &mem, &n_mem, mem_sum_ref);

    if ok {
        println!("--- spike1: all PASS (model A skeleton verified) ---");
        ExitCode::SUCCESS
    } else {
        println!("--- spike1: FAIL ---");
        ExitCode::from(1)
    }
}
