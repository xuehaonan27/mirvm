//! `interp_frame`：tree-walking 解释器。
//!
//! **模型 A 的关键**：`Call` 处**宿主递归**——guest 调用一层，`interp_frame` 递归一层，
//! guest 帧就落在 native（宿主）调用栈上（HotSpot/V8 式），而非独立 VM 帧栈（CPython/Lua 式）。
//! 于是深 guest 递归 = 深 native 递归 = 天然继承 native 栈溢出语义（frame-stack-models.md 的
//! "栈溢出忠实"）。局部数据放 slaved 操作数区（正交于控制流所在的 native 栈）。

use super::bytecode::{BinOp, Operand, Program, Rvalue, Stmt, Terminator};
use super::frame::{OperandRegion, Word};
use super::memory::GuestMemory;

/// 执行环境：只读程序 + slaved 操作数区 + guest 内存。
pub struct Vm<'p> {
    prog: &'p Program,
    region: OperandRegion,
    mem: GuestMemory,
}

impl<'p> Vm<'p> {
    pub fn new(prog: &'p Program) -> Self {
        Vm { prog, region: OperandRegion::new(), mem: GuestMemory::new(1 << 20) }
    }

    /// 从 `func` 入口跑到 `Return`，返回 slot 0。
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
        }
    }

    fn interp_frame(&mut self, func: u32, args: &[Word]) -> Word {
        // 关键：把 `&'p Program` 复制到局部，body/block/stmt 借的是 `'p`（程序活得比 Vm 久），
        // 不是借 `self`——于是循环里对 self.region/self.mem 的可变访问不与之冲突。
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
                Terminator::SwitchInt { discr, targets, otherwise } => {
                    let d = self.eval(base, *discr);
                    blk = targets
                        .iter()
                        .find(|(v, _)| *v == d)
                        .map(|(_, b)| *b)
                        .unwrap_or(*otherwise) as usize;
                }
                Terminator::Call { func: callee, args: aops, dst, target, .. } => {
                    let av: Vec<Word> = aops.iter().map(|o| self.eval(base, *o)).collect();
                    let r = self.interp_frame(*callee, &av); // ← 宿主递归 = guest 帧上 native 栈
                    self.region.write(base, *dst, r);
                    blk = *target as usize;
                }
                Terminator::Return => {
                    let r = self.region.read(base, 0);
                    self.region.restore(base);
                    return r;
                }
                t => unreachable!("spike1 字节码子集不含 unwind 构造: {t:?}"),
            }
        }
    }
}

fn apply_binop(op: BinOp, a: Word, b: Word) -> Word {
    // u64 语义（skeleton 的值都是无符号 word；wrapping 与 native u64 一致）。
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
