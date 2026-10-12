//! Execution-only experiment. The installer adds this module to a disposable source tree.

use crate::vm::{ctx::Engine, interp, ir::*};
use serde::Serialize;
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

mod frontend;

static BODIES: OnceLock<Vec<OnceLock<FuncBody>>> = OnceLock::new();
static CLONES: AtomicU64 = AtomicU64::new(0);
static DROPS: AtomicU64 = AtomicU64::new(0);
static VISITS: AtomicU64 = AtomicU64::new(0);
static PANIC_AT: AtomicU64 = AtomicU64::new(u64::MAX);

pub(crate) fn active() -> bool {
    BODIES.get().is_some()
}
pub(crate) fn published(id: usize) -> Option<&'static FuncBody> {
    BODIES.get()?.get(id)?.get()
}

fn slot(off: u32) -> Slot {
    Slot {
        off,
        width: Width::W64,
    }
}
fn local(off: u32) -> Operand {
    Operand::Slot(slot(off))
}
fn imm(bits: u64) -> Operand {
    Operand::Imm {
        bits,
        width: Width::W64,
    }
}
fn dst(off: u32) -> ScalarPlace {
    ScalarPlace::Slot(slot(off))
}
fn assign(off: u32, rv: Rvalue) -> Stmt {
    Stmt::Assign { dst: dst(off), rv }
}
fn bin(off: u32, op: IntBinOp, a: Operand, b: Operand) -> Stmt {
    assign(
        off,
        Rvalue::IntBin {
            op,
            signed: false,
            a,
            b,
        },
    )
}
fn binding(index: u32) -> Operand {
    Operand::Mem {
        expr: PlaceExpr {
            base: PlaceBase::Local(56),
            steps: vec![PlaceStep::Deref, PlaceStep::Offset((index * 8) as i32)].into(),
        },
        width: Width::W64,
    }
}
fn body(name: &str, blocks: Vec<Block>, dictionary: bool) -> FuncBody {
    let mut params = vec![
        ParamAbi::Scalar(slot(8)),
        ParamAbi::Scalar(slot(16)),
        ParamAbi::Scalar(slot(24)),
    ];
    if dictionary {
        params.push(ParamAbi::Scalar(slot(56)));
    }
    FuncBody {
        frame_size: 128,
        frame_align: 8,
        ret: RetAbi::Scalar(slot(0)),
        params,
        caller_loc_off: None,
        blocks,
        name: name.into(),
    }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
enum Kernel {
    Copy,
    Array,
    CloneDrop,
}

/// The serializable identity contains no process address. Witness addresses are bound on load.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Descriptor {
    stride: u64,
    align: u64,
    length: u64,
    kernel: Kernel,
}

impl Descriptor {
    fn validate(&self) {
        assert!(self.align.is_power_of_two() && self.align <= 64);
        assert!(self.stride <= 128 && self.stride.is_multiple_of(self.align));
        assert!(self.length <= 16384);
        if matches!(self.kernel, Kernel::CloneDrop) {
            assert!(matches!(self.stride, 0 | 64 | 128) && self.align == 64);
        }
    }
    fn bind(&self) -> [u64; 5] {
        self.validate();
        let (clone, drop) = match self.stride {
            0 => (
                clone_value::<0> as *const () as u64,
                drop_value::<0> as *const () as u64,
            ),
            128 => (
                clone_value::<128> as *const () as u64,
                drop_value::<128> as *const () as u64,
            ),
            _ => (
                clone_value::<64> as *const () as u64,
                drop_value::<64> as *const () as u64,
            ),
        };
        [
            self.stride,
            self.length,
            clone,
            drop,
            visit as *const () as u64,
        ]
    }
}

fn indirect(which: u32, args: Vec<Operand>, target: u32, unwind: UnwindAction) -> Terminator {
    Terminator::CallIndirect {
        callee: binding(which),
        args,
        ret: RetDest::Ignore,
        target,
        unwind,
        null_ok: false,
        native_sig: Some(ForeignSig {
            args: match which {
                3 => vec![FfiKind::Ptr],
                2 => vec![FfiKind::Ptr, FfiKind::Ptr],
                _ => vec![FfiKind::Ptr, FfiKind::U64],
            },
            ret: FfiKind::Void,
            fixed: None,
            thunk_args: Vec::new(),
            unwind: true,
        }),
    }
}

/// One owned instruction stream; layouts, const lengths and witnesses stay in its dictionary.
fn template(kernel: Kernel) -> FuncBody {
    let length = if matches!(kernel, Kernel::Array) {
        binding(1)
    } else {
        local(24)
    };
    let mut blocks = vec![
        Block {
            stmts: vec![assign(0, Rvalue::Use(imm(0)))],
            term: Terminator::Goto(1),
        },
        Block {
            stmts: vec![Stmt::Assign {
                dst: ScalarPlace::Slot(Slot {
                    off: 32,
                    width: Width::W8,
                }),
                rv: Rvalue::IntCmp {
                    cc: IntCc::Lt,
                    signed: false,
                    a: local(0),
                    b: length.clone(),
                },
            }],
            term: Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(Operand::Slot(Slot {
                    off: 32,
                    width: Width::W8,
                })),
                targets: vec![(1, 2)],
                otherwise: 5,
            },
        },
        Block {
            stmts: vec![
                bin(40, IntBinOp::Mul, local(0), binding(0)),
                bin(48, IntBinOp::Add, local(8), local(40)),
            ],
            term: Terminator::Goto(3),
        },
        Block {
            stmts: Vec::new(),
            term: Terminator::Goto(4),
        },
        Block {
            stmts: vec![bin(0, IntBinOp::Add, local(0), imm(1))],
            term: Terminator::Goto(1),
        },
        Block {
            stmts: Vec::new(),
            term: Terminator::Return,
        },
    ];
    if matches!(kernel, Kernel::CloneDrop) {
        blocks[2].term = indirect(2, vec![local(16), local(48)], 3, UnwindAction::Continue);
        blocks[3].term = indirect(4, vec![local(16), local(0)], 6, UnwindAction::Cleanup(7));
        blocks.push(Block {
            stmts: Vec::new(),
            term: indirect(3, vec![local(16)], 4, UnwindAction::Continue),
        });
        blocks.push(Block {
            stmts: Vec::new(),
            term: indirect(3, vec![local(16)], 8, UnwindAction::Terminate),
        });
        blocks.push(Block {
            stmts: Vec::new(),
            term: Terminator::Resume,
        });
    } else {
        blocks[3].stmts = vec![
            bin(64, IntBinOp::Add, local(16), local(40)),
            Stmt::MemCopy {
                dst: local(64),
                src: local(48),
                count: binding(0),
                elem_size: 1,
                overlap: false,
            },
        ];
    }
    body("shared_template", blocks, true)
}

fn adapter(name: &str, template_id: u32, dictionary: &[u64; 5], template: &FuncBody) -> FuncBody {
    let mut out = body(
        name,
        vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::Call {
                    callee: template_id,
                    args: vec![
                        local(8),
                        local(16),
                        local(24),
                        imm(dictionary.as_ptr() as u64),
                    ],
                    ret: RetDest::Scalar(dst(0)),
                    target: 1,
                    unwind: UnwindAction::Continue,
                    role: CallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        false,
    );
    out.frame_size = template.frame_size;
    out
}

/// Substitute only the dictionary read form this template exporter emits.
fn specialize(template: &FuncBody, dictionary: &[u64; 5], name: &str) -> FuncBody {
    let ParamAbi::Scalar(dictionary_slot) = template.params.last().unwrap() else {
        panic!("dictionary ABI")
    };
    fn operand(value: &mut Operand, dictionary: &[u64; 5], dictionary_off: u32) {
        if let Operand::Mem {
            expr,
            width: Width::W64,
        } = value
            && matches!(expr.base, PlaceBase::Local(off) if off == dictionary_off)
            && let [PlaceStep::Deref, PlaceStep::Offset(offset)] = &*expr.steps
        {
            assert!(*offset >= 0 && offset % 8 == 0);
            *value = imm(dictionary[*offset as usize / 8]);
        }
    }
    let mut exact = template.clone();
    let operand = |value: &mut Operand, dictionary: &[u64; 5]| {
        operand(value, dictionary, dictionary_slot.off)
    };
    exact.name = name.into();
    exact.params.pop();
    for block in &mut exact.blocks {
        for stmt in &mut block.stmts {
            match stmt {
                Stmt::Assign { rv, .. } => match rv {
                    Rvalue::Use(a) => operand(a, dictionary),
                    Rvalue::IntBin { a, b, .. } | Rvalue::IntCmp { a, b, .. } => {
                        operand(a, dictionary);
                        operand(b, dictionary);
                    }
                    _ => unreachable!("template rvalue outside the experiment"),
                },
                Stmt::MemCopy {
                    dst, src, count, ..
                } => {
                    operand(dst, dictionary);
                    operand(src, dictionary);
                    operand(count, dictionary);
                }
                _ => unreachable!("template statement outside the experiment"),
            }
        }
        match &mut block.term {
            Terminator::CallIndirect { callee, args, .. } => {
                operand(callee, dictionary);
                for arg in args {
                    operand(arg, dictionary);
                }
            }
            Terminator::SwitchInt {
                discr: SwitchDiscr::Scalar(a),
                ..
            } => operand(a, dictionary),
            Terminator::Goto(_) | Terminator::Return | Terminator::Resume => {}
            _ => unreachable!("template terminator outside the experiment"),
        }
    }
    exact
}

#[repr(C, align(64))]
struct Value<const N: usize>([u8; N]);
impl<const N: usize> Clone for Value<N> {
    fn clone(&self) -> Self {
        CLONES.fetch_add(1, Ordering::Relaxed);
        Self(self.0)
    }
}
impl<const N: usize> Drop for Value<N> {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
}
unsafe extern "C-unwind" fn clone_value<const N: usize>(
    out: *mut Value<N>,
    source: *const Value<N>,
) {
    unsafe {
        out.write((&*source).clone());
    }
}
unsafe extern "C-unwind" fn drop_value<const N: usize>(value: *mut Value<N>) {
    unsafe {
        std::ptr::drop_in_place(value);
    }
}
extern "C-unwind" fn visit(_value: *const u8, index: u64) {
    if PANIC_AT.load(Ordering::Relaxed) == index {
        std::panic::panic_any(index);
    }
    VISITS.fetch_add(1, Ordering::Relaxed);
}

#[repr(align(64))]
#[derive(Clone)]
struct Storage([u8; 64]);

#[derive(Default, Serialize)]
struct Costs {
    template_ns: u128,
    descriptors_ns: u128,
    bodies_ns: u128,
    engine_ns: u128,
    specialization_ns: u128,
    verify_ns: u128,
    jit_ns: u128,
    execute_ns: u128,
    first_result_ns: u128,
    total_ns: u128,
    promoted: usize,
    compiled: usize,
    template_bytes: usize,
    descriptors_bytes: usize,
    initial_ir_bytes: usize,
    unique_fragment_bytes: usize,
    checksum: u64,
    unwind_checked: bool,
    audit_ns: u128,
}

fn compile(engine: &Engine, ids: &[u32], costs: &mut Costs) {
    let started = Instant::now();
    let jit = &engine.shared().jit;
    assert!(jit.enabled, "the experiment requires MIRVM_JIT=on");
    for &id in ids {
        assert!(jit.request_compile(id));
    }
    for &id in ids {
        loop {
            let entry = jit.slots[id as usize].load(Ordering::Acquire);
            assert_ne!(entry, crate::vm::jit::FAIL_SENTINEL);
            if entry != 0 {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "JIT did not publish f{id}"
            );
            std::thread::yield_now();
        }
        costs.compiled += 1;
    }
    costs.jit_ns += started.elapsed().as_nanos();
}

fn promote(
    engine: &Engine,
    id: usize,
    template: &FuncBody,
    dictionary: &[u64; 5],
    costs: &mut Costs,
) {
    let shared = engine.shared();
    assert_eq!(shared.jit.slots[id].load(Ordering::Acquire), 0);
    let old = &shared.module.funcs[id];
    let started = Instant::now();
    let exact = specialize(template, dictionary, &old.name);
    costs.specialization_ns += started.elapsed().as_nanos();
    assert_eq!(exact.frame_size, old.frame_size);
    assert_eq!(exact.frame_align, old.frame_align);
    assert_eq!(
        postcard::to_stdvec(&exact.params).unwrap(),
        postcard::to_stdvec(&old.params).unwrap()
    );
    assert_eq!(
        postcard::to_stdvec(&exact.ret).unwrap(),
        postcard::to_stdvec(&old.ret).unwrap()
    );
    let started = Instant::now();
    crate::vm::verify::prototype_body(&shared.module, &shared.instance, &exact).unwrap();
    costs.verify_ns += started.elapsed().as_nanos();
    // Only the serial harness publishes, between complete calls and before any compile request.
    BODIES.get().unwrap()[id].set(exact).unwrap();
    costs.promoted += 1;
    compile(engine, &[id as u32], costs);
}

fn invoke(engine: &Engine, name: &str, args: &[u64]) -> u64 {
    // SAFETY: the harness owns aligned initialized input/output buffers until Engine close.
    unsafe { interp::run_export(engine, name, args) }
        .unwrap()
        .into_returned()
        .unwrap()
        .lo
}

/// Native differential for the same dictionary contract, without any generated IR.
#[inline(never)]
unsafe fn native(
    kernel: Kernel,
    dictionary: &[u64; 5],
    source: *const u8,
    out: *mut u8,
    length: u64,
) -> u64 {
    let length = if matches!(kernel, Kernel::Array) {
        dictionary[1]
    } else {
        length
    };
    for index in 0..length {
        let source = unsafe { source.add((index * dictionary[0]) as usize) };
        if matches!(kernel, Kernel::CloneDrop) {
            // SAFETY: bind supplies these exact C-unwind signatures and concrete layouts.
            let clone: unsafe extern "C-unwind" fn(*mut u8, *const u8) =
                unsafe { std::mem::transmute(dictionary[2]) };
            let drop: unsafe extern "C-unwind" fn(*mut u8) =
                unsafe { std::mem::transmute(dictionary[3]) };
            unsafe {
                clone(out, source);
            }
            struct Guard {
                out: *mut u8,
                drop: unsafe extern "C-unwind" fn(*mut u8),
            }
            impl Drop for Guard {
                fn drop(&mut self) {
                    unsafe {
                        (self.drop)(self.out);
                    }
                }
            }
            let guard = Guard { out, drop };
            visit(out, index);
            std::hint::black_box(&guard);
        } else {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source,
                    out.add((index * dictionary[0]) as usize),
                    dictionary[0] as usize,
                );
            }
        }
    }
    length
}

pub fn main() {
    let total = Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).is_some_and(|arg| arg == "export") {
        frontend::run(&argv[2..]);
        return;
    }
    assert!(
        matches!(argv.len(), 10 | 11),
        "mode kernel stride length instances active rounds work_threshold check_unwind [export.json]"
    );
    let exported: Option<frontend::Export> = argv
        .get(10)
        .map(|path| serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap());
    let mode = argv[1].as_str();
    assert!(matches!(
        mode,
        "native" | "shared" | "eager" | "tiered" | "shared-jit" | "eager-jit"
    ));
    let kernel = match argv[2].as_str() {
        "copy" => Kernel::Copy,
        "array" => Kernel::Array,
        "clone" => Kernel::CloneDrop,
        _ => panic!("kernel"),
    };
    let values: Vec<u64> = argv[3..10].iter().map(|s| s.parse().unwrap()).collect();
    let (stride, length, instances, used, rounds, threshold, unwind) = (
        values[0],
        values[1],
        values[2] as usize,
        values[3] as usize,
        values[4],
        values[5],
        values[6] != 0,
    );
    assert!(used > 0 && used <= instances && instances <= 16384 && rounds > 0);
    let mut costs = Costs::default();
    let started = Instant::now();
    let generic = exported
        .as_ref()
        .map_or_else(|| template(kernel), |export| export.template.clone());
    costs.template_ns = started.elapsed().as_nanos();
    costs.template_bytes = postcard::to_stdvec(&generic).unwrap().len();
    let started = Instant::now();
    let descriptors: Vec<_> = exported.as_ref().map_or_else(
        || {
            (0..instances)
                .map(|id| {
                    let stride = if stride == 999 {
                        if matches!(kernel, Kernel::CloneDrop) {
                            [0, 64, 128][id % 3]
                        } else {
                            [0, 1, 8, 64, 128][id % 5]
                        }
                    } else {
                        stride
                    };
                    Descriptor {
                        stride,
                        align: if matches!(kernel, Kernel::CloneDrop) {
                            64
                        } else {
                            stride.clamp(1, 64).next_power_of_two()
                        },
                        length,
                        kernel,
                    }
                })
                .collect()
        },
        |export| export.descriptors.clone(),
    );
    assert_eq!(descriptors.len(), instances);
    let dictionaries: Vec<Box<[u64; 5]>> = descriptors.iter().map(|d| Box::new(d.bind())).collect();
    costs.descriptors_ns = started.elapsed().as_nanos();
    let audit = Instant::now();
    let encoded = postcard::to_stdvec(&descriptors).unwrap();
    costs.descriptors_bytes = encoded.len();
    let decoded: Vec<Descriptor> = postcard::from_bytes(&encoded).unwrap();
    assert_eq!(
        decoded.iter().map(Descriptor::bind).collect::<Vec<_>>(),
        dictionaries.iter().map(|d| **d).collect::<Vec<_>>()
    );
    let generic: FuncBody = postcard::from_bytes(&postcard::to_stdvec(&generic).unwrap()).unwrap();
    costs.audit_ns += audit.elapsed().as_nanos();

    let names: Vec<_> = (0..instances).map(|i| format!("instance_{i}")).collect();
    let started = Instant::now();
    let mut funcs: Vec<_> = dictionaries
        .iter()
        .zip(&names)
        .map(|(dictionary, name)| {
            if mode.starts_with("eager") {
                specialize(&generic, dictionary, name)
            } else {
                adapter(name, instances as u32, dictionary, &generic)
            }
        })
        .collect();
    funcs.push(generic.clone());
    costs.bodies_ns = started.elapsed().as_nanos();
    let audit = Instant::now();
    costs.initial_ir_bytes = postcard::to_stdvec(&funcs).unwrap().len();
    let mut fragments = std::collections::HashMap::new();
    for function in &funcs {
        let bytes = crate::vm::ir::frag::encode(function).unwrap();
        fragments.entry(blake3::hash(&bytes)).or_insert(bytes.len());
    }
    costs.unique_fragment_bytes = fragments.values().sum();
    costs.audit_ns += audit.elapsed().as_nanos();
    let module = Module {
        funcs: funcs.into(),
        exports: names
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone().into(), i as u32))
            .collect(),
        ..Module::default()
    };
    let started = Instant::now();
    // SAFETY: descriptors and callbacks outlive Engine, and all bodies use the stated raw ABI.
    let engine = unsafe { Engine::from_module_unchecked(module) }.unwrap();
    costs.engine_ns = started.elapsed().as_nanos();
    BODIES
        .set((0..=instances).map(|_| OnceLock::new()).collect())
        .unwrap();
    let max_stride = descriptors.iter().map(|d| d.stride).max().unwrap();
    let max_length = descriptors.iter().map(|d| d.length).max().unwrap();
    let mut input =
        vec![Storage([0; 64]); ((max_stride * max_length) as usize).div_ceil(64).max(1)];
    for (index, block) in input.iter_mut().enumerate() {
        for (at, byte) in block.0.iter_mut().enumerate() {
            *byte = (index * 17 + at * 31) as u8;
        }
    }
    let mut output = vec![Storage([0; 64]); input.len().max(2)];
    let mut oracle = output.clone();
    let source = input.as_ptr().cast::<u8>();
    // SAFETY: every Value<N> is an initialized byte array with alignment 64, including the ZST.
    assert_eq!(
        unsafe {
            native(
                kernel,
                &dictionaries[0],
                source,
                oracle.as_mut_ptr().cast(),
                descriptors[0].length,
            )
        },
        descriptors[0].length
    );
    CLONES.store(0, Ordering::Relaxed);
    DROPS.store(0, Ordering::Relaxed);
    VISITS.store(0, Ordering::Relaxed);
    if mode == "eager-jit" {
        compile(&engine, &(0..used as u32).collect::<Vec<_>>(), &mut costs);
    }
    if mode == "shared-jit" {
        compile(&engine, &[instances as u32], &mut costs);
    }
    let mut work = vec![0u64; used];
    for _ in 0..rounds {
        for id in 0..used {
            let length = descriptors[id].length;
            let args = [
                source as u64,
                output.as_mut_ptr() as u64,
                if matches!(kernel, Kernel::Array) {
                    0
                } else {
                    length
                },
            ];
            if mode == "tiered" && work[id] >= threshold && published(id).is_none() {
                promote(&engine, id, &generic, &dictionaries[id], &mut costs);
            }
            let started = Instant::now();
            let result = if mode == "native" {
                unsafe {
                    native(
                        kernel,
                        &dictionaries[id],
                        source,
                        output.as_mut_ptr().cast(),
                        length,
                    )
                }
            } else {
                invoke(&engine, &names[id], &args)
            };
            costs.execute_ns += started.elapsed().as_nanos();
            assert_eq!(result, length);
            costs.checksum = costs.checksum.wrapping_add(result);
            work[id] = work[id].saturating_add(length.max(1));
            if costs.first_result_ns == 0 {
                costs.first_result_ns = total.elapsed().as_nanos() - costs.audit_ns;
            }
            if (stride == 999 || exported.is_some()) && !matches!(kernel, Kernel::CloneDrop) {
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        output.as_ptr().cast::<u8>(),
                        (length * descriptors[id].stride) as usize,
                    )
                };
                let expected = unsafe { std::slice::from_raw_parts(source, bytes.len()) };
                assert_eq!(bytes, expected);
            }
        }
    }
    if matches!(kernel, Kernel::CloneDrop) {
        assert_eq!(
            CLONES.load(Ordering::Relaxed),
            length * used as u64 * rounds
        );
        assert_eq!(
            DROPS.load(Ordering::Relaxed),
            CLONES.load(Ordering::Relaxed)
        );
        assert_eq!(
            VISITS.load(Ordering::Relaxed),
            CLONES.load(Ordering::Relaxed)
        );
    } else {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                output.as_ptr().cast::<u8>(),
                (descriptors[used - 1].length * descriptors[used - 1].stride) as usize,
            )
        };
        let expected = unsafe { std::slice::from_raw_parts(source, bytes.len()) };
        assert_eq!(bytes, expected);
        costs.checksum = bytes.iter().fold(costs.checksum, |sum, b| {
            sum.wrapping_mul(31).wrapping_add(*b as u64)
        });
    }
    if unwind {
        let length = descriptors[0].length;
        let args = [source as u64, output.as_mut_ptr() as u64, length];
        assert!(matches!(kernel, Kernel::CloneDrop) && length > 1);
        std::panic::set_hook(Box::new(|_| {}));
        PANIC_AT.store(length / 2, Ordering::Relaxed);
        CLONES.store(0, Ordering::Relaxed);
        DROPS.store(0, Ordering::Relaxed);
        VISITS.store(0, Ordering::Relaxed);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if mode == "native" {
                unsafe {
                    native(
                        kernel,
                        &dictionaries[0],
                        source,
                        output.as_mut_ptr().cast(),
                        length,
                    );
                }
            } else {
                invoke(&engine, &names[0], &args);
            }
        }));
        assert!(caught.is_err());
        assert_eq!(*caught.unwrap_err().downcast::<u64>().unwrap(), length / 2);
        assert_eq!(CLONES.load(Ordering::Relaxed), length / 2 + 1);
        assert_eq!(
            DROPS.load(Ordering::Relaxed),
            CLONES.load(Ordering::Relaxed)
        );
        PANIC_AT.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(invoke(&engine, &names[0], &args), length);
        costs.unwind_checked = true;
    }
    if mode == "shared" || mode == "eager" {
        assert!(
            engine
                .shared()
                .jit
                .slots
                .iter()
                .all(|s| s.load(Ordering::Acquire) == 0)
        );
    }
    engine.close();
    engine.wait_closed().unwrap();
    costs.total_ns = total.elapsed().as_nanos();
    println!(
        "{}",
        serde_json::json!({"mode": mode, "kernel": argv[2], "stride": stride, "length": length,
        "instances": instances, "active": used, "rounds": rounds, "threshold": threshold, "exported": exported.is_some(), "costs": costs})
    );
}
