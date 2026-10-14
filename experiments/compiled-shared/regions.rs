//! Experimental shared machine code over a concrete activation's operand frame.
//!
//! Internal entries have a private ABI. Guest slots keep their concrete calling convention, so a
//! specialized body can replace the shared path at the next call without changing a live frame.

#[path = "shared_policy.rs"]
mod policy;
pub(crate) use policy::Sample;

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::vm::ctx::{Ctx, Shared};
use crate::vm::ir::*;
use crate::vm::jit::{CodeDomain, FAIL_SENTINEL};

#[derive(Default)]
pub(crate) struct Regions {
    policy: Option<policy::Policy>,
    targets: Vec<Option<u32>>,
    representatives: Vec<u32>,
    compiled_instances: Vec<AtomicBool>,
    adapter: Option<u32>,
    calls: bool,
    specialize: bool,
    prepare_ns: u128,
    translated: AtomicU64,
    translate_ns: AtomicU64,
    entries: AtomicU64,
    exits: AtomicU64,
    bridges: AtomicU64,
    ledger: bool,
}

impl Regions {
    pub(crate) fn prepare(module: &mut Module, domain: CodeDomain) -> Self {
        if !crate::options::shared_regions() || !crate::options::jit() {
            return Self::default();
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
            calls: crate::options::shared_region_calls(),
            specialize: crate::options::shared_region_specialize(),
            ledger: crate::options::jit_ledger(),
            ..Self::default()
        };
        // Archive owners keep these addresses stable for the Engine's lifetime. No descriptor is
        // decoded and no bytecode is hashed here. Different archive copies remain separate groups.
        let mut groups: HashMap<usize, Vec<u32>> = HashMap::new();
        let mut index = 0u32;
        for part in module.funcs.parts() {
            match part {
                FuncPart::Owned(bodies) => {
                    for body in bodies {
                        if let Some(shared) = &body.shared {
                            let key = std::ptr::from_ref(shared.template.as_ref()) as usize;
                            groups.entry(key).or_default().push(index);
                        }
                        index += 1;
                    }
                }
                FuncPart::Archived(archive) => {
                    for body in archive.bodies() {
                        if let Some(shared) = body.shared.as_ref() {
                            let key = std::ptr::from_ref(shared.template.as_ref()) as usize;
                            groups.entry(key).or_default().push(index);
                        }
                        index += 1;
                    }
                }
            }
        }
        let mut groups: Vec<_> = groups.into_values().filter(|ids| ids.len() > 1).collect();
        groups.sort_unstable_by_key(|ids| ids[0]);
        let mut headers = Vec::with_capacity(groups.len() + 1);
        for ids in groups {
            let target = u32::try_from(count + headers.len()).unwrap();
            state.representatives.push(ids[0]);
            for id in ids {
                state.targets[id as usize] = Some(target);
            }
            // Only the private ABI is reserved. The worker builds its actual frame and guarded
            // entry from translated IR; no guest caller can address this placeholder body.
            headers.push(header(target));
        }
        state.adapter = Some(u32::try_from(count + headers.len()).unwrap());
        let mut adapter = header(state.adapter.unwrap());
        adapter.params.truncate(2);
        headers.push(adapter);
        module.funcs.append(headers.into());
        if crate::options::shared_region_profile() || crate::options::shared_region_policy() {
            assert!(
                state.calls && state.specialize,
                "work policy requires call bridges and concrete promotion"
            );
            state.policy = Some(policy::Policy::new(&state));
        }
        state.prepare_ns = started.elapsed().as_nanos();
        state
    }

    pub(crate) fn contains(&self, func: u32) -> bool {
        self.targets.get(func as usize).is_some_and(Option::is_some)
    }

    pub(crate) fn is_adapter(&self, func: u32) -> bool {
        self.adapter == Some(func)
    }

    pub(crate) fn body(&self, shared: &Shared, func: u32) -> Option<FuncBody> {
        let group = (func as usize).checked_sub(self.targets.len())?;
        let representative = *self.representatives.get(group)?;
        let started = Instant::now();
        let original = shared
            .module
            .funcs
            .execution(representative as usize)
            .unwrap();
        let template = &original.shared.as_ref().unwrap().template;
        let body = translate(template, func, self.calls.then_some(self.adapter.unwrap()));
        crate::vm::verify::generic_template(&body, shared.module.funcs.len()).unwrap();
        self.translated.fetch_add(1, Ordering::Relaxed);
        self.translate_ns
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        Some(body)
    }

    /// Published concrete entries win at dispatch. Requests keep their original identity; only
    /// shared nominations address an internal slot, so the two tiers never overwrite each other.
    #[inline]
    pub(crate) fn interpret(&self, ctx: *mut Ctx, func: u32) -> bool {
        if !self.contains(func) {
            return false;
        }
        if !self.specialize {
            return true;
        }
        let jit = unsafe { &(*ctx).shared.jit };
        let entry = jit.slots[func as usize].load(Ordering::Acquire);
        if entry != 0 && entry != FAIL_SENTINEL {
            return false;
        }
        assert_ne!(entry, FAIL_SENTINEL, "concrete entry failed compilation");
        if self.policy.as_ref().is_some_and(|policy| policy.enforce) {
            return true;
        }
        let count = jit.counters[func as usize].fetch_add(1, Ordering::Relaxed) + 1;
        if count == jit.threshold {
            jit.request_compile(func);
        }
        true
    }

    pub(crate) fn iterations(&self, ctx: *mut Ctx, func: u32, count: u32) -> bool {
        let Some(target) = self.targets.get(func as usize).copied().flatten() else {
            return false;
        };
        if let Some(policy) = &self.policy
            && policy.enforce
        {
            policy.iterations(ctx, func, target, count);
            return true;
        }
        let jit = unsafe { &(*ctx).shared.jit };
        jit.note_iterations(target, count);
        if self.specialize {
            jit.note_iterations(func, count);
        }
        true
    }

    pub(crate) fn select(&self, ctx: *mut Ctx, func: u32) -> u64 {
        let Some(target) = self.targets.get(func as usize).copied().flatten() else {
            return 0;
        };
        let jit = unsafe { &(*ctx).shared.jit };
        let slot = &jit.slots[target as usize];
        let mut entry = slot.load(Ordering::Acquire);
        if let Some(policy) = &self.policy
            && policy.enforce
        {
            policy.nominate(ctx, func, target, entry);
        } else if entry == 0 {
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

    pub(crate) fn sample(&self, func: u32, compiled: bool) -> Option<Sample<'_>> {
        let target = self.targets.get(func as usize).copied().flatten()?;
        self.policy
            .as_ref()?
            .sample(func, target as usize - self.targets.len(), compiled)
    }

    pub(crate) fn compile_start(
        &self,
        shared: &Shared,
        func: u32,
    ) -> Result<Option<policy::Ticket>, ()> {
        self.policy
            .as_ref()
            .map_or(Ok(None), |policy| policy.start(self, shared, func))
    }

    pub(crate) fn compile_complete(
        &self,
        shared: &Shared,
        func: u32,
        ticket: Option<policy::Ticket>,
    ) {
        if let Some(ticket) = ticket {
            self.policy.as_ref().unwrap().complete(shared, func, ticket);
        }
    }

    pub(crate) fn report(&self, shared: &Shared) {
        if !self.ledger || self.targets.is_empty() {
            return;
        }
        if let Some(policy) = &self.policy {
            policy.report(self, shared);
        }
        let compiled = |id: usize| {
            let entry = shared.jit.slots[id].load(Ordering::Acquire);
            entry != 0 && entry != FAIL_SENTINEL
        };
        let first = self.targets.len();
        let end = first + self.representatives.len();
        let code_bytes: u64 = shared
            .jit
            .symbol_ranges()
            .iter()
            .filter(|symbol| (first..end).contains(&(symbol.func_id as usize)))
            .map(|symbol| symbol.size)
            .sum();
        crate::diag::instrument(format_args!(
            "mirvm-shared-regions: instances={} templates={} translated={} compiled={} concrete={} entered_instances={} entries={} exits={} bridges={} prepare_ns={} translate_ns={} code_bytes={}",
            self.targets.iter().filter(|v| v.is_some()).count(),
            self.representatives.len(),
            self.translated.load(Ordering::Relaxed),
            (first..end).filter(|&id| compiled(id)).count(),
            self.targets
                .iter()
                .enumerate()
                .filter(|(id, target)| target.is_some() && compiled(*id))
                .count(),
            self.compiled_instances
                .iter()
                .filter(|v| v.load(Ordering::Relaxed))
                .count(),
            self.entries.load(Ordering::Relaxed),
            self.exits.load(Ordering::Relaxed),
            self.bridges.load(Ordering::Relaxed),
            self.prepare_ns,
            self.translate_ns.load(Ordering::Relaxed),
            code_bytes,
        ));
    }
}

pub(crate) struct Activation<'a> {
    ctx: *mut Ctx,
    base: usize,
    body: &'a FuncBody,
    edge: &'a Cell<Option<Bb>>,
    sample: Option<&'a Sample<'a>>,
}

/// The descriptor is stack-owned by enter, including across callbacks. The original raw catch
/// boundary still owns cleanup; this C-unwind helper sets that activation's edge before calling.
pub(crate) unsafe extern "C-unwind" fn call(activation: *const Activation<'_>, block: u64) -> u64 {
    let activation = unsafe { &*activation };
    let state = unsafe { &(*activation.ctx).shared.shared_regions };
    if let Some(sample) = activation.sample {
        sample.statements(0);
    }
    if state.ledger {
        state.bridges.fetch_add(1, Ordering::Relaxed);
    }
    crate::vm::interp::exec_call(
        activation.ctx,
        activation.base,
        activation.edge,
        activation
            .body
            .term(&activation.body.code()[block as usize]),
        activation.sample,
    ) as u64
}

/// Select code once per activation; neither a shared publication nor a concrete upgrade transfers
/// an already running interpreter frame into another execution form.
pub(crate) fn enter(
    ctx: *mut Ctx,
    entry: u64,
    base: usize,
    block: usize,
    body: &FuncBody,
    edge: &Cell<Option<Bb>>,
    sample: Option<&Sample<'_>>,
) -> usize {
    let state = unsafe { &(*ctx).shared.shared_regions };
    if state.ledger {
        state.exits.fetch_add(1, Ordering::Relaxed);
    }
    let activation = Activation {
        ctx,
        base,
        body,
        edge,
        sample,
    };
    let args = [
        base as u64,
        block as u64,
        std::ptr::from_ref(&activation) as u64,
    ];
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

fn header(id: u32) -> FuncBody {
    FuncBody {
        name: format!("shared_region_{id}").into(),
        frame_size: 32,
        frame_align: 8,
        ret: RetAbi::Scalar(slot(24)),
        params: vec![
            ParamAbi::Scalar(slot(0)),
            ParamAbi::Scalar(slot(8)),
            ParamAbi::Scalar(slot(16)),
        ],
        caller_loc_off: None,
        shared: None,
        blocks: vec![Block {
            stmts: Vec::new(),
            term: Terminator::Unreachable,
        }],
    }
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

fn translate(template: &GenericTemplate, id: u32, adapter: Option<u32>) -> FuncBody {
    let mut tr = Translate {
        next: 32,
        size: 32,
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
    let mut continuations = Vec::new();
    for (bb, block) in template.blocks.iter().enumerate() {
        let mut stmts = Vec::new();
        for stmt in &block.stmts {
            tr.next = 32;
            let stmt = tr.statement(stmt);
            stmts.append(&mut tr.prefix);
            stmts.push(stmt);
        }
        tr.next = 32;
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
            Terminator::Call { target, .. } if adapter.is_some() => {
                let continuation = (template.blocks.len() + 2 + continuations.len()) as u32;
                continuations.push(Block {
                    stmts: Vec::new(),
                    term: Terminator::SwitchInt {
                        discr: SwitchDiscr::Scalar(Operand::Slot(slot(8))),
                        targets: vec![(u128::from(*target), target + 1)],
                        otherwise: 0,
                    },
                });
                Terminator::Call {
                    callee: adapter.unwrap(),
                    args: vec![
                        Operand::Slot(slot(16)),
                        Operand::Imm {
                            bits: bb as u64,
                            width: Width::W64,
                        },
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(slot(8))),
                    target: continuation,
                    unwind: UnwindAction::Continue,
                    role: CallRole::Normal,
                }
            }
            _ => {
                stmts.push(Stmt::Assign {
                    dst: ScalarPlace::Slot(slot(24)),
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
    blocks.extend(continuations);
    FuncBody {
        frame_size: tr.size,
        blocks,
        ..header(id)
    }
}
