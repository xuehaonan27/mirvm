//! Experimental shared machine code over a concrete activation's operand frame.
//!
//! Compiled regions stop before calls and other non-branch terminators. The original interpreter
//! handles those boundaries, including instance-specific call ABIs, cleanup and returns.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::vm::ctx::{Ctx, Shared};
use crate::vm::ir::*;
use crate::vm::jit::{CodeDomain, FAIL_SENTINEL};

#[derive(Default)]
pub(crate) struct Regions {
    targets: Vec<Option<u32>>,
    compiled_instances: Vec<AtomicBool>,
    template_ids: Vec<u32>,
    prepare_ns: u128,
    entries: AtomicU64,
    exits: AtomicU64,
    ledger: bool,
}

impl Regions {
    pub(crate) fn prepare(
        module: &mut Module,
        domain: CodeDomain,
    ) -> Result<Self, crate::error::Error> {
        if !crate::options::shared_regions() || !crate::options::jit() {
            return Ok(Self::default());
        }
        assert_eq!(
            domain,
            CodeDomain::Plain,
            "the experiment admits only plain execution"
        );
        assert!(
            crate::options::no_jit_cache(),
            "the experiment requires MIRVM_NO_JIT_CACHE=1"
        );
        assert!(
            !crate::options::jit_reload(),
            "experimental frame entries cannot be reloaded"
        );
        assert!(
            !crate::options::generic_payback(),
            "measure the frame tier independently of the budget"
        );
        let started = Instant::now();
        let count = module.funcs.len();
        let mut state = Self {
            targets: vec![None; count],
            compiled_instances: (0..count).map(|_| AtomicBool::new(false)).collect(),
            ledger: crate::options::jit_ledger(),
            ..Self::default()
        };
        // Inspect archived tags before decoding. Ordinary std functions stay archived.
        let mut ids = Vec::new();
        let mut index = 0usize;
        for part in module.funcs.parts() {
            match part {
                FuncPart::Owned(bodies) => {
                    for body in bodies {
                        if body.shared.is_some() {
                            ids.push(index);
                        }
                        index += 1;
                    }
                }
                FuncPart::Archived(archive) => {
                    for body in archive.bodies() {
                        if body.shared.is_some() {
                            ids.push(index);
                        }
                        index += 1;
                    }
                }
            }
        }
        let mut groups = HashMap::new();
        let mut bodies = Vec::new();
        for id in ids {
            let shared = module.funcs.execution(id).unwrap().shared.as_ref().unwrap();
            let key = *blake3::hash(&postcard::to_stdvec(&shared.template).unwrap()).as_bytes();
            let target = if let Some(target) = groups.get(&key) {
                *target
            } else {
                let target = u32::try_from(count + bodies.len()).unwrap();
                let body = translate(&shared.template, target);
                // The result is ordinary IR with no guest references; the existing verifier and
                // JIT consume it without a second code generator or a weakened artifact check.
                crate::vm::verify::generic_template(&body, 0)?;
                bodies.push(body);
                groups.insert(key, target);
                state.template_ids.push(target);
                target
            };
            state.targets[id] = Some(target);
        }
        module.funcs.append(bodies.into());
        state.prepare_ns = started.elapsed().as_nanos();
        Ok(state)
    }

    pub(crate) fn contains(&self, func: u32) -> bool {
        self.targets.get(func as usize).is_some_and(Option::is_some)
    }

    pub(crate) fn target(&self, func: u32) -> u32 {
        self.targets
            .get(func as usize)
            .copied()
            .flatten()
            .unwrap_or(func)
    }

    pub(crate) fn select(&self, ctx: *mut Ctx, func: u32) -> u64 {
        let Some(target) = self.targets.get(func as usize).copied().flatten() else {
            return 0;
        };
        let jit = unsafe { &(*ctx).shared.jit };
        let slot = &jit.slots[target as usize];
        let mut entry = slot.load(Ordering::Acquire);
        if entry == 0 {
            let count = jit.counters[target as usize].fetch_add(1, Ordering::Relaxed) + 1;
            if count == jit.threshold {
                jit.request_compile(target);
            }
            if jit.sync && count >= jit.threshold {
                let start = Instant::now();
                while entry == 0 {
                    assert!(
                        start.elapsed() < Duration::from_secs(30),
                        "shared entry did not compile"
                    );
                    std::thread::yield_now();
                    entry = slot.load(Ordering::Acquire);
                }
            }
        }
        assert_ne!(entry, FAIL_SENTINEL, "shared entry failed compilation");
        if entry != 0 && self.ledger {
            self.entries.fetch_add(1, Ordering::Relaxed);
            self.compiled_instances[func as usize].store(true, Ordering::Relaxed);
        }
        entry
    }

    pub(crate) fn report(&self, shared: &Shared) {
        if !self.ledger || self.targets.is_empty() {
            return;
        }
        let compiled = self
            .template_ids
            .iter()
            .filter(|&&id| {
                let entry = shared.jit.slots[id as usize].load(Ordering::Acquire);
                entry != 0 && entry != FAIL_SENTINEL
            })
            .count();
        let symbols = shared.jit.symbol_ranges();
        let code_bytes: u64 = symbols
            .iter()
            .filter(|symbol| self.template_ids.contains(&symbol.func_id))
            .map(|symbol| symbol.size)
            .sum();
        crate::diag::instrument(format_args!(
            "mirvm-shared-regions: instances={} templates={} compiled={} entered_instances={} entries={} exits={} prepare_ns={} code_bytes={}",
            self.targets.iter().filter(|v| v.is_some()).count(),
            self.template_ids.len(),
            compiled,
            self.compiled_instances
                .iter()
                .filter(|v| v.load(Ordering::Relaxed))
                .count(),
            self.entries.load(Ordering::Relaxed),
            self.exits.load(Ordering::Relaxed),
            self.prepare_ns,
            code_bytes,
        ));
    }
}

/// The entry is selected once per normal activation. A loop already running in the interpreter
/// does not transfer into code that finishes compiling later.
pub(crate) fn enter(ctx: *mut Ctx, entry: u64, base: usize, block: usize) -> usize {
    let state = unsafe { &(*ctx).shared.shared_regions };
    if state.ledger {
        state.exits.fetch_add(1, Ordering::Relaxed);
    }
    let args = [base as u64, block as u64];
    let mut result = [0u64; 2];
    if crate::os_arch::PLAIN_CODE_PINS_CTX {
        type Entry = extern "C-unwind" fn(*mut Ctx, *const u64, *mut u64);
        let call: Entry = unsafe { std::mem::transmute(entry as usize) };
        call(ctx, args.as_ptr(), result.as_mut_ptr());
    } else {
        type Entry = extern "C-unwind" fn(*const u64, *mut u64);
        let call: Entry = unsafe { std::mem::transmute(entry as usize) };
        call(args.as_ptr(), result.as_mut_ptr());
    }
    result[0] as usize
}

fn slot(off: u32) -> Slot {
    Slot {
        off,
        width: Width::W64,
    }
}

fn address(off: u32) -> PlaceExpr {
    PlaceExpr {
        base: PlaceBase::Local(0),
        steps: vec![
            PlaceStep::Deref,
            PlaceStep::Offset(i32::try_from(off).unwrap()),
        ]
        .into(),
    }
}

struct Translate {
    next: u32,
    size: u32,
    prefix: Vec<Stmt>,
}

impl Translate {
    fn place(&mut self, place: &PlaceExpr) -> PlaceExpr {
        let PlaceBase::Local(off) = place.base else {
            unreachable!("verified shared place");
        };
        let mut out = address(off);
        let mut steps = out.steps.into_vec();
        for step in place.steps.iter() {
            steps.push(match step {
                PlaceStep::Deref | PlaceStep::Offset(_) => step.clone(),
                PlaceStep::IndexScaled { idx, stride } => {
                    let temp = Slot {
                        off: self.next,
                        width: idx.width,
                    };
                    self.next += 8;
                    self.size = self.size.max(self.next);
                    self.prefix.push(Stmt::Assign {
                        dst: ScalarPlace::Slot(temp),
                        rv: Rvalue::Use(Operand::Mem {
                            expr: address(idx.off),
                            width: idx.width,
                        }),
                    });
                    PlaceStep::IndexScaled {
                        idx: temp,
                        stride: *stride,
                    }
                }
                _ => unreachable!("verified shared place step"),
            });
        }
        out.steps = steps.into();
        out
    }

    fn operand(&mut self, op: &Operand) -> Operand {
        match op {
            Operand::Slot(slot) => Operand::Mem {
                expr: address(slot.off),
                width: slot.width,
            },
            Operand::Mem { expr, width } => Operand::Mem {
                expr: self.place(expr),
                width: *width,
            },
            Operand::AddrOf(expr) => Operand::AddrOf(self.place(expr)),
            Operand::Imm { .. } => op.clone(),
            _ => unreachable!("verified shared operand"),
        }
    }

    fn destination(&mut self, dst: &ScalarPlace) -> ScalarPlace {
        match dst {
            ScalarPlace::Slot(slot) => ScalarPlace::Mem {
                expr: address(slot.off),
                width: slot.width,
            },
            ScalarPlace::Mem { expr, width } => ScalarPlace::Mem {
                expr: self.place(expr),
                width: *width,
            },
        }
    }

    fn rvalue(&mut self, rv: &Rvalue) -> Rvalue {
        match rv {
            Rvalue::Use(a) => Rvalue::Use(self.operand(a)),
            Rvalue::Cast { a, to, from } => Rvalue::Cast {
                a: self.operand(a),
                to: *to,
                from: *from,
            },
            Rvalue::IntBin { op, signed, a, b } => Rvalue::IntBin {
                op: *op,
                signed: *signed,
                a: self.operand(a),
                b: self.operand(b),
            },
            Rvalue::IntCmp { cc, signed, a, b } => Rvalue::IntCmp {
                cc: *cc,
                signed: *signed,
                a: self.operand(a),
                b: self.operand(b),
            },
            Rvalue::Ref(expr) => Rvalue::Ref(self.place(expr)),
            _ => unreachable!("verified shared rvalue"),
        }
    }

    fn statement(&mut self, stmt: &Stmt) -> Stmt {
        match stmt {
            Stmt::Assign { dst, rv } => Stmt::Assign {
                dst: self.destination(dst),
                rv: self.rvalue(rv),
            },
            Stmt::MemCopy {
                src,
                dst,
                count,
                elem_size,
                overlap,
            } => Stmt::MemCopy {
                src: self.operand(src),
                dst: self.operand(dst),
                count: self.operand(count),
                elem_size: *elem_size,
                overlap: *overlap,
            },
            _ => unreachable!("verified shared statement"),
        }
    }
}

fn translate(template: &GenericTemplate, id: u32) -> FuncBody {
    let mut tr = Translate {
        next: 24,
        size: 24,
        prefix: Vec::new(),
    };
    let mut blocks = vec![Block {
        stmts: Vec::new(),
        term: Terminator::SwitchInt {
            discr: SwitchDiscr::Scalar(Operand::Slot(slot(8))),
            targets: (0..template.blocks.len())
                .map(|bb| (bb as u128, bb as u32 + 1))
                .collect(),
            otherwise: template.blocks.len() as u32 + 1,
        },
    }];
    for (bb, block) in template.blocks.iter().enumerate() {
        let mut stmts = Vec::new();
        for stmt in &block.stmts {
            tr.next = 24;
            let stmt = tr.statement(stmt);
            stmts.append(&mut tr.prefix);
            stmts.push(stmt);
        }
        tr.next = 24;
        let term = match &block.term {
            Terminator::Goto(target) => Terminator::Goto(target + 1),
            Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(discr),
                targets,
                otherwise,
            } => {
                let discr = SwitchDiscr::Scalar(tr.operand(discr));
                stmts.append(&mut tr.prefix);
                Terminator::SwitchInt {
                    discr,
                    targets: targets
                        .iter()
                        .map(|(value, target)| (*value, target + 1))
                        .collect(),
                    otherwise: otherwise + 1,
                }
            }
            _ => {
                stmts.push(Stmt::Assign {
                    dst: ScalarPlace::Slot(slot(16)),
                    rv: Rvalue::Use(Operand::Imm {
                        bits: bb as u64,
                        width: Width::W64,
                    }),
                });
                Terminator::Return
            }
        };
        blocks.push(Block { stmts, term });
    }
    blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Unreachable,
    });
    FuncBody {
        name: format!("shared_region_{id}").into(),
        frame_size: tr.size,
        frame_align: 8,
        ret: RetAbi::Scalar(slot(16)),
        params: vec![ParamAbi::Scalar(slot(0)), ParamAbi::Scalar(slot(8))],
        caller_loc_off: None,
        shared: None,
        blocks,
    }
}
