//! `interp_frame`: tree-walking interpreter.
//!
//! The key to model A: a `Call` recurses in the host -- one guest call, one `interp_frame`
//! recursion -- so guest frames sit on the native (host) call stack (HotSpot/V8 style)
//! rather than on a separate VM frame stack (CPython/Lua style). Deep guest recursion is
//! therefore deep native recursion and inherits native stack-overflow semantics for free.
//! Local data lives in the slaved operand region, orthogonal to the native stack that
//! carries control flow.

use super::bytecode::{BinOp, Operand, Program, Rvalue, Stmt, Terminator};
use super::frame::{OperandRegion, Word};
use super::memory::GuestMemory;

/// Execution environment: read-only program + slaved operand region + guest memory.
pub struct Vm<'p> {
    prog: &'p Program,
    region: OperandRegion,
    mem: GuestMemory,
}

impl<'p> Vm<'p> {
    pub fn new(prog: &'p Program) -> Self {
        Vm {
            prog,
            region: OperandRegion::new(),
            mem: GuestMemory::new(1 << 20),
        }
    }

    /// Run from the `func` entry to `Return`; returns slot 0.
    pub fn run(&mut self, func: u32, args: &[Word]) -> Word {
        self.interp_frame(func, args)
    }

    #[inline]
    fn eval(&self, base: usize, op: Operand) -> Word {
        match op {
            Operand::Slot(s) => self.region.read(base, s),
            Operand::Const(c) => c,
        }
    }

    fn eval_rvalue(&mut self, base: usize, rv: &Rvalue) -> Word {
        match rv {
            Rvalue::Use(op) => self.eval(base, *op),
            Rvalue::Binary(op, l, r) => {
                let a = self.eval(base, *l);
                let b = self.eval(base, *r);
                apply_binop(*op, a, b)
            }
            Rvalue::Alloc(size) => {
                let sz = self.eval(base, *size);
                self.mem.alloc(sz)
            }
            Rvalue::Load(ptr) => {
                let addr = self.eval(base, *ptr);
                unsafe { self.mem.load(addr) }
            }
            rv => unreachable!("spike1 bytecode subset has no concurrency constructors: {rv:?}"),
        }
    }

    fn interp_frame(&mut self, func: u32, args: &[Word]) -> Word {
        // Copy `&'p Program` into a local so body/block/stmt borrow `'p` (the program
        // outlives the Vm) rather than `self`; mutable access to self.region/self.mem in the
        // loop then does not conflict with them.
        let prog = self.prog;
        let body = &prog.funcs[func as usize];

        let base = self.region.reserve(body.num_slots);
        for (i, a) in args.iter().enumerate() {
            self.region.write(base, (i + 1) as u32, *a); // slot 0=ret, 1..=args
        }

        let mut blk = 0usize;
        loop {
            let block = &body.blocks[blk];
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Assign(dst, rv) => {
                        let v = self.eval_rvalue(base, rv);
                        self.region.write(base, *dst, v);
                    }
                    Stmt::Store(ptr, val) => {
                        let addr = self.eval(base, *ptr);
                        let v = self.eval(base, *val);
                        unsafe { self.mem.store(addr, v) };
                    }
                }
            }
            match &block.term {
                Terminator::Goto(t) => blk = *t as usize,
                Terminator::SwitchInt {
                    discr,
                    targets,
                    otherwise,
                } => {
                    let d = self.eval(base, *discr);
                    blk = targets
                        .iter()
                        .find(|(v, _)| *v == d)
                        .map(|(_, b)| *b)
                        .unwrap_or(*otherwise) as usize;
                }
                Terminator::Call {
                    func: callee,
                    args: aops,
                    dst,
                    target,
                    ..
                } => {
                    let av: Vec<Word> = aops.iter().map(|o| self.eval(base, *o)).collect();
                    // Host recursion: a guest frame is a native stack frame.
                    let r = self.interp_frame(*callee, &av);
                    self.region.write(base, *dst, r);
                    blk = *target as usize;
                }
                Terminator::Return => {
                    let r = self.region.read(base, 0);
                    self.region.restore(base);
                    return r;
                }
                t => unreachable!("spike1 bytecode subset has no unwind constructors: {t:?}"),
            }
        }
    }
}

fn apply_binop(op: BinOp, a: Word, b: Word) -> Word {
    // u64 semantics: skeleton values are all unsigned words; wrapping matches native u64.
    match op {
        BinOp::Add => a.wrapping_add(b),
        BinOp::Sub => a.wrapping_sub(b),
        BinOp::Mul => a.wrapping_mul(b),
        BinOp::Lt => (a < b) as u64,
        BinOp::Le => (a <= b) as u64,
        BinOp::Eq => (a == b) as u64,
        BinOp::Gt => (a > b) as u64,
        BinOp::Ge => (a >= b) as u64,
    }
}
