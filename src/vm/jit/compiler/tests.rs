//! Standing checks for the compilation pipeline: domain isolation, the trace boundary
//! pin/restore contract, and symbol registration.

use super::*;

/// A compiler for a test that drives the pipeline directly.
///
/// The store is off: a body these tests build can share a fragment with a real program, and a cache
/// hit would replace the compile the test is about. The store's own paths are covered by its unit
/// tests and by the `fib32-jit-cache` gate.
fn test_compiler(shared: &Shared, domain: CodeDomain) -> Compiler<'_> {
    let mut compiler = Compiler::with_tier(shared, domain, Tier::Optimized);
    compiler.jit_cache = false;
    compiler
}

/// The trace domain must compile the same guest IR as the plain domain.
/// The domain may only change how recorder state is addressed (the pinned
/// register); if it changed guest codegen, the trace and plain runs of one
/// program would diverge, which no gate would catch until the domain is
/// wired to activation entry. Compiling the same body in both domains and
/// requiring a published entry from each is the cheapest standing check.
#[test]
fn both_code_domains_compile_the_same_body() {
    for domain in [CodeDomain::Plain, CodeDomain::Trace] {
        let shared = Shared::new(ir::Module {
            funcs: vec![body("domain_body", Terminator::Return)].into(),
            ..ir::Module::default()
        });
        let mut compiler = test_compiler(&shared, domain);
        compiler.compile(0);
        // Each domain publishes into its own slot set; the trace entry must
        // not appear in the plain slots, which is the isolation the split
        // exists for.
        let own = shared.jit.slots_for(domain).slots_fast[0].load(Ordering::Acquire);
        assert_ne!(
            own, 0,
            "{domain:?} published no fast entry in its own slots"
        );
        let other = match domain {
            CodeDomain::Plain => {
                shared.jit.slots_for(CodeDomain::Trace).slots_fast[0].load(Ordering::Acquire)
            }
            CodeDomain::Trace => {
                shared.jit.slots_for(CodeDomain::Plain).slots_fast[0].load(Ordering::Acquire)
            }
        };
        assert_eq!(
            other, 0,
            "{domain:?} leaked an entry into the other domain's slots"
        );
    }
}

/// The trace domain always pins its recorder; the plain domain pins the thread's `Ctx` exactly
/// where the pair can carry it across compiled frames, so the two domains are separate ISAs
/// wherever the pin is used at all.
#[test]
fn the_pinned_register_follows_the_pair_and_the_domain() {
    let plain = isa_for(CodeDomain::Plain, Tier::Optimized);
    assert_eq!(
        plain.flags().enable_pinned_reg(),
        crate::os_arch::PLAIN_CODE_PINS_CTX,
        "plain code pins the context only where the pair can carry it"
    );
    let trace = isa_for(CodeDomain::Trace, Tier::Optimized);
    assert!(
        trace.flags().enable_pinned_reg(),
        "trace code must reserve {} for the recorder",
        crate::arch::PINNED_REG
    );

    // The trace ISA must build on this host with the reservation actually in
    // place: which register it is belongs to the architecture, not to a target
    // triple, so the invariant is the setting rather than the host's name.
    let isa = trace_domain_isa();
    assert!(
        isa.flags().enable_pinned_reg(),
        "the trace ISA on {} must reserve {} for the producer",
        isa.triple(),
        crate::arch::PINNED_REG
    );
}

/// A trace body that reports the pinned register instead of running guest
/// code. It uses the packed trace ABI, because that is the shape the boundary
/// calls; no guest program can express a register read, which is exactly why
/// the boundary's two obligations are checked with it here.
fn define_pin_probe(compiler: &mut Compiler<'_>) -> u64 {
    let mut sig = compiler.module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    let id = compiler
        .module
        .declare_function("mirvm_test_read_pinned", Linkage::Local, &sig)
        .unwrap();
    let mut cctx = compiler.module.make_context();
    cctx.func.signature = sig;
    {
        let mut b = FunctionBuilder::new(&mut cctx.func, &mut compiler.fbc);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let ret = b.block_params(entry)[1];
        let pinned = b.ins().get_pinned_reg(types::I64);
        b.ins().store(MemFlagsData::trusted(), pinned, ret, 0);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize();
    }
    compiler.module.define_function(id, &mut cctx).unwrap();
    compiler.module.clear_context(&mut cctx);
    compiler.module.finalize_definitions().unwrap();
    compiler.module.get_finalized_function(id) as u64
}

/// A body that unwinds instead of returning. Rust's own calling convention
/// matches the packed trace ABI, and its frames carry unwind tables, so this
/// drives the boundary's landing pad with a real unwind rather than a
/// simulation of one.
unsafe extern "C-unwind" fn unwind_through_boundary(_args: *const u64, _ret: *mut u64) {
    panic!("trace boundary unwind probe");
}

/// Observe the caller's reserved register immediately after the inner boundary returns or
/// unwinds. Rust may reuse a callee-saved register between statements, so two reads around a
/// Rust `catch_unwind` do not establish the ABI value at the boundary itself.
fn define_restore_probe(compiler: &mut Compiler<'_>, enter: u64, recorder: u64) -> u64 {
    use cranelift_codegen::ir::{
        BlockArg, BlockCall, ExceptionTableData, ExceptionTableItem, ExceptionTag,
    };
    let mut signature = compiler.module.make_signature();
    signature.params.extend([AbiParam::new(types::I64); 2]);
    let id = compiler
        .module
        .declare_function("mirvm_test_restore_pinned", Linkage::Local, &signature)
        .unwrap();
    let mut context = compiler.module.make_context();
    context.func.signature = signature;
    {
        let mut b = FunctionBuilder::new(&mut context.func, &mut compiler.fbc);
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let arguments = b.block_params(entry)[0];
        let result = b.block_params(entry)[1];
        let body = b
            .ins()
            .load(types::I64, MemFlagsData::trusted(), arguments, 0);
        let boundary = b.ins().iconst(types::I64, enter as i64);
        let recorder = b.ins().iconst(types::I64, recorder as i64);
        let mut inner_signature = compiler.module.make_signature();
        inner_signature
            .params
            .extend([AbiParam::new(types::I64); 4]);
        let signature = b.func.import_signature(inner_signature);
        let normal = b.create_block();
        let unwind = b.create_block();
        b.append_block_param(unwind, types::I64);
        let normal_call = BlockCall::new(
            normal,
            std::iter::empty::<BlockArg>(),
            &mut b.func.dfg.value_lists,
        );
        let unwind_call = b.func.dfg.block_call(unwind, &[BlockArg::TryCallExn(0)]);
        let table = b.func.dfg.exception_tables.push(ExceptionTableData::new(
            signature,
            normal_call,
            [ExceptionTableItem::Tag(
                ExceptionTag::with_number(0).unwrap(),
                unwind_call,
            )],
        ));
        b.ins()
            .try_call_indirect(boundary, &[recorder, body, arguments, result], table);
        b.switch_to_block(normal);
        let restored = b.ins().get_pinned_reg(types::I64);
        b.ins().store(MemFlagsData::trusted(), restored, result, 8);
        b.ins().return_(&[]);
        b.switch_to_block(unwind);
        let exception = b.block_params(unwind)[0];
        let restored = b.ins().get_pinned_reg(types::I64);
        b.ins().store(MemFlagsData::trusted(), restored, result, 8);
        let resume = compiler
            .module
            .declare_func_in_func(compiler.unwind_resume, b.func);
        b.ins().call(resume, &[exception]);
        b.ins().trap(TrapCode::user(1).unwrap());
        b.seal_all_blocks();
        b.finalize();
    }
    compiler.module.define_function(id, &mut context).unwrap();
    let unwind = context
        .compiled_code()
        .unwrap()
        .create_unwind_info(compiler.module.isa())
        .unwrap()
        .unwrap();
    let lsda = build_lsda(&collect_call_sites(&context));
    compiler.pending_unwind.push((id, unwind, Some(lsda)));
    compiler.module.clear_context(&mut context);
    compiler.module.finalize_definitions().unwrap();
    compiler.register_pending_eh_frames();
    compiler.module.get_finalized_function(id) as u64
}

#[test]
fn trace_entry_pins_the_recorder_and_restores_it_even_when_it_unwinds() {
    const CALLER: u64 = 0x4341_4c4c_4552;
    const RECORDER: u64 = 0x5052_4f44_5543_4552;
    let shared = Shared::new(ir::Module {
        funcs: vec![body("domain_body", Terminator::Return)].into(),
        ..ir::Module::default()
    });
    let mut compiler = test_compiler(&shared, CodeDomain::Trace);
    let enter = shared.jit.trace_enter.load(Ordering::Acquire);
    assert_ne!(enter, 0);
    let read_pin = define_pin_probe(&mut compiler);
    let observe = define_restore_probe(&mut compiler, enter, RECORDER);
    let mut result = [0; 2];
    unsafe {
        crate::vm::jit::call_trace_body(enter, CALLER as *mut _, observe, &[read_pin], &mut result);
    }
    assert_eq!(
        result,
        [RECORDER, CALLER],
        "install and normal-path restore"
    );

    result = [0; 2];
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        crate::vm::jit::call_trace_body(
            enter,
            CALLER as *mut _,
            observe,
            &[unwind_through_boundary as *const u8 as u64],
            &mut result,
        )
    }));
    assert!(caught.is_err(), "the guest unwind must reach the host");
    assert_eq!(
        result[1], CALLER,
        "unwind-path restore before the caller resumes"
    );
}

/// The syscall number the probe body carries.
///
/// The value is never used: the body is compiled and never run, and what is under test is that the
/// trace domain accepts a `HostSyscallTrace` call site at all. It is therefore spelled once here
/// rather than taken from the platform, whose numbering is its own and whose C library bindings do
/// not name one for every platform this build supports.
const HOST_GETPID_SYSCALL: u64 = 0;

/// The trace domain's own syscall site must actually compile. If the pinned
/// lowering were rejected, the body would silently stay interpreted
/// and the differential gates would still pass while the feature did
/// nothing -- so the published trace entry is the assertion.
#[test]
fn trace_domain_compiles_a_pinned_syscall_body() {
    let mut probe = body(
        "pinned_syscall",
        Terminator::CallBuiltin {
            builtin: ir::Builtin::HostSyscallTrace,
            args: vec![Operand::Imm {
                bits: HOST_GETPID_SYSCALL,
                width: Width::W64,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: ir::BuiltinCallRole::Normal,
        },
    );
    probe.ret = RetAbi::Zst;
    let shared = Shared::new(ir::Module {
        funcs: vec![probe].into(),
        ..ir::Module::default()
    });
    let mut compiler = test_compiler(&shared, CodeDomain::Trace);
    compiler.compile(0);
    assert_ne!(
        shared.jit.slots_for(CodeDomain::Trace).slots[0].load(Ordering::Acquire),
        0,
        "the trace domain rejected a body containing its own syscall site"
    );
}

pub(super) fn body(name: &str, first: Terminator) -> ir::FuncBody {
    ir::FuncBody {
        shared: None,
        prepared: Default::default(),
        objects: Vec::new(),
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            ir::Block {
                stmts: Vec::new(),
                term: first,
            },
            ir::Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: name.into(),
    }
}

#[test]
fn dereferenced_pointer_slots_stay_in_ssa_unless_their_address_escapes() {
    let slot = |off| Slot {
        off,
        width: Width::W64,
    };
    let local = |off, steps: Vec<ir::PlaceStep>| ir::PlaceExpr {
        base: ir::PlaceBase::Local(off),
        steps: steps.into(),
    };
    for escaped in [false, true] {
        let mut probe = body("pointer_slot", Terminator::Return);
        probe.frame_size = 40;
        probe.params = vec![
            ParamAbi::Scalar(slot(0)),
            ParamAbi::Scalar(slot(8)),
            ParamAbi::Scalar(slot(16)),
        ];
        probe.ret = RetAbi::Scalar(slot(24));
        if escaped {
            // Change the first parameter through a pointer to its slot. Subsequent dereferences
            // must read the updated pointer in memory, not the parameter's original SSA value.
            probe.blocks[0].stmts.extend([
                Stmt::Assign {
                    dst: ScalarPlace::Slot(slot(32)),
                    rv: ir::Rvalue::Ref(local(0, vec![])),
                },
                Stmt::Assign {
                    dst: ScalarPlace::Mem {
                        expr: local(32, vec![ir::PlaceStep::Deref]),
                        width: Width::W64,
                    },
                    rv: ir::Rvalue::Use(Operand::Slot(slot(8))),
                },
            ]);
        }
        probe.blocks[0].stmts.push(Stmt::Assign {
            dst: ScalarPlace::Slot(slot(24)),
            rv: ir::Rvalue::Use(Operand::Mem {
                expr: local(
                    0,
                    vec![
                        ir::PlaceStep::Deref,
                        ir::PlaceStep::Deref,
                        ir::PlaceStep::IndexScaled {
                            idx: slot(16),
                            stride: 8,
                        },
                    ],
                ),
                width: Width::W64,
            }),
        });
        assert_eq!(analyze_frame(&probe).needs_frame(), escaped);
        let shared = Shared::new(ir::Module {
            funcs: vec![probe].into(),
            ..ir::Module::default()
        });
        let mut compiler = test_compiler(&shared, CodeDomain::Plain);
        compiler.compile(0);
        let fast = shared
            .jit
            .symbol_ranges()
            .into_iter()
            .find(|range| range.role == JitSymbolRole::FastBody)
            .expect("pointer probe compiled");
        let call: extern "C" fn(*const *const u64, *const *const u64, u64) -> u64 =
            unsafe { std::mem::transmute(fast.start as usize) };
        let first = [11u64, 23, 47];
        let second = [101u64, 211, 431];
        for index in 0..first.len() {
            assert_eq!(
                call(&first.as_ptr(), &second.as_ptr(), index as u64),
                if escaped { second[index] } else { first[index] },
            );
        }
        // Registered unwind records refer to the module's code for the rest of the process.
        std::mem::forget(compiler);
    }
}

#[test]
fn constant_copies_preserve_overlap_alignment_and_artifact_reloading() {
    for size in (0..=33).chain([64]) {
        for (count, elem_size) in [
            (None, 1),
            (
                Some(Operand::Imm {
                    bits: size,
                    width: Width::W64,
                }),
                1,
            ),
            (
                Some(Operand::Imm {
                    bits: size,
                    width: Width::W8,
                }),
                2,
            ),
            (
                Some(Operand::Slot(Slot {
                    off: 16,
                    width: Width::W64,
                })),
                1,
            ),
        ] {
            let bytes = size * elem_size;
            let dynamic = matches!(count, Some(Operand::Slot(_)));
            for tier in [Tier::Baseline, Tier::Optimized] {
                let mut probe = body("constant_copy", Terminator::Return);
                probe.frame_size = 24;
                probe.params = [0, 8, 16]
                    .map(|off| {
                        ParamAbi::Scalar(Slot {
                            off,
                            width: Width::W64,
                        })
                    })
                    .into();
                let pointer = |off| ir::PlaceExpr {
                    base: ir::PlaceBase::Local(off),
                    steps: vec![ir::PlaceStep::Deref].into(),
                };
                probe.blocks[0].stmts.push(match &count {
                    None => Stmt::Copy {
                        dst: pointer(0),
                        src: pointer(8),
                        size: size as u32,
                    },
                    Some(count) => Stmt::MemCopy {
                        dst: Operand::Slot(Slot {
                            off: 0,
                            width: Width::W64,
                        }),
                        src: Operand::Slot(Slot {
                            off: 8,
                            width: Width::W64,
                        }),
                        count: count.clone(),
                        elem_size,
                        overlap: true,
                    },
                });
                let shared = Shared::new(ir::Module {
                    funcs: vec![probe].into(),
                    ..ir::Module::default()
                });
                let mut compiler = Compiler::with_tier(&shared, CodeDomain::Plain, tier);
                compiler.jit_cache = false;
                compiler.reload = true;
                compiler.compile(0);
                let entry = compiler
                    .last_entry
                    .as_ref()
                    .expect("copy body was captured");
                let entry = artifact::Entry::decode(&entry.encode().unwrap()).unwrap();
                let fast = entry.symbol(JitSymbolRole::FastBody).unwrap();
                let calls_memmove = fast.relocs.iter().any(
                    |r| matches!(&r.target, artifact::Target::Named(name) if &**name == "memmove"),
                );
                assert_eq!(
                    calls_memmove,
                    dynamic || bytes > 32,
                    "{tier:?} copy count={count:?}, elem_size={elem_size}"
                );
                let ordinals = artifact::Ordinals::of(&shared.module.funcs[0]).unwrap();
                let linked =
                    artifact::link(&entry, &[JitSymbolRole::FastBody], |target| match target {
                        artifact::Target::Named(name) => imports::whitelist()
                            .get(name.as_ref())
                            .map(|addr| *addr as u64),
                        target => artifact::target_value(&shared, 0, &ordinals, target),
                    })
                    .expect("copy body links from its artifact");
                let copy: extern "C" fn(*mut u8, *const u8, u64) = unsafe {
                    std::mem::transmute(linked.entry(JitSymbolRole::FastBody).unwrap() as usize)
                };
                for (src, dst) in [(1, 2), (2, 1), (1, 1), (1, 137)] {
                    let mut actual = std::array::from_fn::<_, 272, _>(|i| i as u8);
                    let mut expected = actual;
                    expected.copy_within(src..src + bytes as usize, dst);
                    copy(
                        unsafe { actual.as_mut_ptr().add(dst) },
                        unsafe { actual.as_ptr().add(src) },
                        size,
                    );
                    assert_eq!(
                        actual, expected,
                        "{tier:?} copy count={count:?}, elem_size={elem_size}, src={src}, dst={dst}"
                    );
                }
                // Registered unwind records refer to the module's code for the rest of the process.
                std::mem::forget(compiler);
            }
        }
    }
}

#[test]
fn real_compilation_registers_every_executable_role() {
    let mut caller = body(
        "profile_caller",
        Terminator::Call {
            callee: 1,
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: ir::CallRole::Normal,
        },
    );
    caller.blocks.insert(
        1,
        ir::Block {
            stmts: Vec::new(),
            term: Terminator::Call {
                callee: 2,
                args: Vec::new(),
                ret: RetDest::Ignore,
                target: 2,
                unwind: UnwindAction::Continue,
                role: ir::CallRole::Normal,
            },
        },
    );
    let callee = body("profile_callee", Terminator::Return);
    let second_callee = body("profile_second_callee", Terminator::Return);
    let shared = Shared::new(ir::Module {
        funcs: vec![caller, callee, second_callee].into(),
        ..ir::Module::default()
    });
    let mut compiler = test_compiler(&shared, CodeDomain::Plain);

    compiler.compile(0);

    assert_ne!(shared.jit.slots[0].load(Ordering::Acquire), 0);
    let ranges = shared.jit.symbol_ranges();
    let published = shared.jit.slots_fast[0].load(Ordering::Acquire);
    assert!(
        ranges
            .iter()
            .any(|range| { range.start == published && range.role == JitSymbolRole::FastBody }),
        "compiled callers must enter the body after checking headroom themselves"
    );
    assert_eq!(ranges.len(), 4);
    assert!(ranges.iter().any(|range| {
        range.func_id == 0 && range.role == JitSymbolRole::FastBody && range.size != 0
    }));
    assert!(ranges.iter().any(|range| {
        range.func_id == 0 && range.role == JitSymbolRole::Packed && range.size != 0
    }));
    for callee in [1, 2] {
        let entry = shared.jit.slots_fast[callee].load(Ordering::Acquire);
        assert_ne!(entry, 0);
        assert!(ranges.iter().any(|range| {
            range.func_id == callee as u32
                && range.role == JitSymbolRole::C2i
                && range.start == entry
                && range.size != 0
        }));
    }
    assert_eq!(
        ranges
            .iter()
            .filter(|range| shared.jit.guest_func_at(range.start) == Some(range.func_id))
            .count(),
        1,
        "only the fast body may become a MIRVM guest backtrace frame"
    );

    // Published code and registered unwind metadata have process lifetime
    // in production; keep that same lifetime in this direct compiler test.
    std::mem::forget(compiler);
}

#[test]
fn failed_request_cannot_leak_symbols_into_the_next_compile_batch() {
    for domain in [CodeDomain::Plain, CodeDomain::Trace] {
        for role in [
            JitSymbolRole::FastBody,
            JitSymbolRole::Packed,
            JitSymbolRole::C2i,
        ] {
            let caller = |name| {
                body(
                    name,
                    Terminator::Call {
                        callee: 2,
                        args: Vec::new(),
                        ret: RetDest::Ignore,
                        target: 1,
                        unwind: UnwindAction::Continue,
                        role: ir::CallRole::Normal,
                    },
                )
            };
            let shared = Shared::new(ir::Module {
                funcs: vec![
                    caller("failed_caller"),
                    caller("successful_caller"),
                    body("shared_callee", Terminator::Return),
                    body("independent_target", Terminator::Return),
                ]
                .into(),
                ..ir::Module::default()
            });
            let mut compiler = test_compiler(&shared, domain);
            compiler.reload = true;
            compiler.fail_after_symbol = Some(role);
            compiler.compile(0);
            let slots = shared.jit.slots_for(domain);
            assert_eq!(slots.slots[0].load(Ordering::Acquire), 0);
            assert_eq!(slots.slots_fast[2].load(Ordering::Acquire), 0);
            assert!(shared.jit.symbol_ranges().is_empty());

            compiler.fail_after_symbol = None;
            compiler.compile(3);
            assert_ne!(slots.slots[3].load(Ordering::Acquire), 0);
            assert_eq!(slots.slots_fast[2].load(Ordering::Acquire), 0);
            let ranges = shared.jit.symbol_ranges();
            assert_eq!(ranges.len(), 2);
            assert!(ranges.iter().all(|range| range.func_id == 3));

            // This caller needs the same trampoline as the failed request. Its definition may
            // already be finalized, but must remain reusable without publishing the failed body.
            compiler.compile(1);
            assert_ne!(slots.slots[1].load(Ordering::Acquire), 0);
            assert_ne!(slots.slots_fast[2].load(Ordering::Acquire), 0);
            assert_eq!(slots.slots[0].load(Ordering::Acquire), 0);
            let ranges = shared.jit.symbol_ranges();
            assert_eq!(ranges.len(), 5);
            assert!(ranges.iter().all(|range| range.func_id != 0));
            assert_eq!(
                ranges
                    .iter()
                    .filter(|range| range.role == JitSymbolRole::C2i)
                    .count(),
                1
            );
            assert!(compiler.pending_callees.is_empty());
            assert!(compiler.artifacts.is_empty());
            std::mem::forget(compiler);
        }
    }
}

#[test]
fn previous_artifact_encoding_is_refused() {
    let shared = Shared::new(ir::Module {
        funcs: vec![body("encoding_probe", Terminator::Return)].into(),
        ..Default::default()
    });
    let mut compiler = test_compiler(&shared, CodeDomain::Plain);
    compiler.reload = true;
    compiler.compile(0);
    let mut bytes = compiler.last_entry.as_ref().unwrap().encode().unwrap();
    assert!(artifact::Entry::decode(&bytes).is_ok());
    bytes[0] = 2;
    assert!(artifact::Entry::decode(&bytes).is_err());
    std::mem::forget(compiler);
}

/// The in-process reload proof (`jit-code-cache-design.md` §5 step 2): compile one body, serialize
/// it, link the artifact back and run *that* code.
///
/// The differential is the module's own entry for the same body: both must compute the frozen word
/// the immediate names plus the added constant. The published entry must be the linked one, so a
/// linker that quietly republished the compile session's code fails here rather than passing.
#[test]
fn a_compiled_body_runs_from_its_artifact() {
    const FROZEN: u64 = 0x6a00_0000_1234;
    const ADDED: u64 = 7;
    let slot = || ir::Slot {
        off: 0,
        width: ir::Width::W64,
    };
    let body = ir::FuncBody {
        shared: None,
        prepared: Default::default(),
        objects: Vec::new(),
        frame_size: 16,
        frame_align: 8,
        ret: ir::RetAbi::Scalar(slot()),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![ir::Block {
            stmts: vec![
                ir::Stmt::Assign {
                    dst: ir::ScalarPlace::Slot(slot()),
                    rv: ir::Rvalue::Use(ir::Operand::AddrImm(ir::LinkAddr(FROZEN))),
                },
                ir::Stmt::Assign {
                    dst: ir::ScalarPlace::Slot(slot()),
                    rv: ir::Rvalue::IntBin {
                        op: ir::IntBinOp::Add,
                        signed: false,
                        a: ir::Operand::Slot(slot()),
                        b: ir::Operand::Imm {
                            bits: ADDED,
                            width: ir::Width::W64,
                        },
                    },
                },
            ],
            term: ir::Terminator::Return,
        }],
        name: "reload_probe".into(),
    };
    let shared = Shared::new(ir::Module {
        funcs: vec![body].into(),
        ..ir::Module::default()
    });
    let mut compiler = test_compiler(&shared, CodeDomain::Plain);
    // Collect the artifact, and on a pair whose call relocation this linker applies, also publish what
    // it links back (the compiler path); the body-level link below is what every pair proves.
    compiler.reload = true;
    compiler.compile(0);

    let published = shared.jit.slots_fast[0].load(Ordering::Acquire);
    assert_ne!(published, 0, "no entry was published");
    assert!(
        shared
            .jit
            .recorded_sites(CodeDomain::Plain, 0)
            .is_some_and(|sites| sites > 0),
        "the body recorded no site, so the reload would prove nothing"
    );
    if cfg!(target_arch = "x86_64") {
        let module_entry = match compiler.module.get_name("f0") {
            Some(cranelift_module::FuncOrDataId::Func(id)) => {
                compiler.module.get_finalized_function(id) as u64
            }
            other => panic!("the module has no body entry for f0: {other:?}"),
        };
        assert_ne!(
            published, module_entry,
            "the published entry is the module's own code, so nothing was linked back"
        );
    }

    // What a store would hold: the encoded bytes, decoded again.
    let captured = compiler.last_entry.as_ref().expect("the body was captured");
    let entry = artifact::Entry {
        fragment: captured.fragment,
        jit: captured.jit.clone(),
        symbols: vec![
            captured
                .symbol(JitSymbolRole::FastBody)
                .expect("the body symbol was captured")
                .clone(),
        ],
        adapters: Vec::new(),
    };
    let bytes = entry.encode().expect("an artifact encodes");
    let entry = artifact::Entry::decode(&bytes).expect("an artifact decodes");
    // The body alone: it names the frozen word and nothing else, so its relocation table is what every
    // pair applies.
    // The body polls signals at every block entry, and that helper reads the calling thread's Engine
    // activation. The link is what this test proves, so the poll resolves to a no-op; every other name
    // comes from the one import table, as it does in the compile path.
    extern "C" fn probe_poll() {}
    let ordinals = artifact::Ordinals::of(&shared.module.funcs[0]).expect("the body has ordinals");
    let linked = artifact::link(&entry, &[JitSymbolRole::FastBody], |target| match target {
        artifact::Target::Named(name) if name.as_ref() == "mirvm_poll_signals" => {
            Some(probe_poll as *const u8 as u64)
        }
        artifact::Target::Named(name) => imports::whitelist()
            .get(name.as_ref())
            .map(|addr| *addr as u64),
        target => artifact::target_value(&shared, 0, &ordinals, target),
    })
    .expect("the probe body links back");

    // A fast body takes the guest fast ABI, which for this body is one scalar result. The module's own
    // body is not called for comparison: it polls signals, and that helper needs an Engine activation
    // the test does not build, so the interpreter's arithmetic is the reference here.
    type Fast = extern "C" fn() -> u64;
    let linked: Fast =
        unsafe { std::mem::transmute(linked.entry(JitSymbolRole::FastBody).unwrap()) };
    assert_eq!(
        linked(),
        FROZEN + ADDED,
        "the linked code did not compute the frozen word plus the immediate"
    );
    // Published code lives to process end in production; keep that same lifetime here.
    std::mem::forget(compiler);
}

/// The store's precondition: one body's artifact is the same bytes in two programs that number its
/// functions and place its frozen addresses differently. Everything a program decides — a function id,
/// a link address, a stub address, a PLT slot — is recorded as the fragment's own reference ordinal,
/// so an entry written by one program is readable by the next.
#[test]
fn an_entry_depends_on_the_body_and_not_on_the_program() {
    /// The probe reads a frozen link address, a guest thread-local and calls its callee through the
    /// PLT slot: one fragment reference of each kind this test needs, in the canonical order the walk
    /// gives them. The frozen address, the callee id and the TLS id are all the program's, so none of
    /// them may reach the entry.
    fn probe(frozen: u64, callee: u32, tls: u32) -> ir::FuncBody {
        let slot = || ir::Slot {
            off: 0,
            width: ir::Width::W64,
        };
        ir::FuncBody {
            shared: None,
            prepared: Default::default(),
            objects: Vec::new(),
            frame_size: 16,
            frame_align: 8,
            ret: ir::RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![
                ir::Block {
                    stmts: vec![
                        ir::Stmt::Assign {
                            dst: ir::ScalarPlace::Slot(slot()),
                            rv: ir::Rvalue::Use(ir::Operand::AddrImm(ir::LinkAddr(frozen))),
                        },
                        ir::Stmt::Assign {
                            dst: ir::ScalarPlace::Slot(slot()),
                            rv: ir::Rvalue::TlsRef(tls),
                        },
                    ],
                    term: ir::Terminator::Call {
                        callee,
                        args: Vec::new(),
                        ret: ir::RetDest::Ignore,
                        target: 1,
                        unwind: ir::UnwindAction::Continue,
                        role: ir::CallRole::Normal,
                    },
                },
                ir::Block {
                    stmts: Vec::new(),
                    term: ir::Terminator::Return,
                },
            ],
            name: "artifact_probe".into(),
        }
    }

    /// Compile one function of one program and hand back the entry it captured.
    fn entry_of(funcs: Vec<ir::FuncBody>, which: u32, warm_context: bool) -> artifact::Entry {
        let shared = Shared::new(ir::Module {
            funcs: funcs.into(),
            ..ir::Module::default()
        });
        let mut compiler = test_compiler(&shared, CodeDomain::Plain);
        compiler.reload = true;
        if warm_context {
            compiler.compile(1 - which);
        }
        compiler.compile(which);
        let entry = compiler
            .last_entry
            .clone()
            .expect("the function was captured");
        std::mem::forget(compiler);
        entry
    }

    let callee = |frame_size| {
        let mut body = body("artifact_callee", Terminator::Return);
        body.frame_size = frame_size;
        body
    };
    // The same fragment at function 0 with its callee at 1 and TLS slot 3, and at function 1 with its
    // callee at 0 and TLS slot 8; the frozen address differs too, because a program places its data
    // where it likes and numbers its thread-locals as it meets them.
    let first = entry_of(vec![probe(0x6a00_0000_1000, 1, 3), callee(8)], 0, false);
    let second = entry_of(
        vec![callee(1 << 20), probe(0x6a00_0000_9000, 0, 8)],
        1,
        false,
    );
    let reused = entry_of(vec![probe(0x6a00_0000_1000, 1, 3), callee(8)], 0, true);
    assert_eq!(
        first.encode().unwrap(),
        reused.encode().unwrap(),
        "compilation scratch must not carry instructions or relocations between definitions"
    );

    assert!(
        first.symbol(JitSymbolRole::FastBody).is_some(),
        "the compared entries hold the body, or the comparison proves nothing"
    );
    assert!(
        first.symbols.iter().any(|symbol| symbol
            .relocs
            .iter()
            .any(|reloc| matches!(reloc.target, artifact::Target::Tls(_)))),
        "the entry names no TLS site, so nothing replays the id the loading session has"
    );
    let stack_need = first
        .symbols
        .iter()
        .flat_map(|symbol| &symbol.relocs)
        .find_map(|reloc| {
            matches!(reloc.target, artifact::Target::StackNeed(_)).then_some(&reloc.target)
        })
        .expect("the caller must record its callee's stack requirement");
    for frame_size in [8, 1 << 20] {
        let shared = Shared::new(ir::Module {
            funcs: vec![probe(0x6a00_0000_1000, 1, 3), callee(frame_size)].into(),
            ..Default::default()
        });
        let ordinals = artifact::Ordinals::of(&shared.module.funcs[0]).unwrap();
        assert_eq!(
            artifact::target_value(&shared, 0, &ordinals, stack_need),
            Some(u64::from(frame_size) + STACK_GUARD_MARGIN as u64)
        );
    }
    assert_eq!(
        first.fragment, second.fragment,
        "the same body is not the same fragment in two programs"
    );

    assert_eq!(
        first.encode().expect("the entry encodes"),
        second.encode().expect("the entry encodes"),
        "the entry carries the program that compiled it"
    );
}

/// A native signature an *indirect* call carries is a recorded site, not a raw pointer into the
/// compiling process's IR.
///
/// The signature lives in this process's heap (a `ForeignSig` inside the module), so an entry that
/// baked its address would be a wild pointer in every other process — exactly the class the entry's
/// canonical form exists to prevent. Two things are checked: the captured code carries no such
/// address, and the entry names the site, which is what lets the next process resolve it by
/// re-matching the body.
#[test]
fn an_indirect_native_signature_is_recorded_and_replayed() {
    use crate::vm::ir::{FfiKind, ForeignSig};
    use crate::vm::jit::reloc::{Body, SigPart};

    let signature = ForeignSig {
        args: vec![FfiKind::I32],
        ret: FfiKind::Void,
        fixed: None,
        thunk_args: Vec::new(),
        unwind: false,
    };
    let funcs = vec![ir::FuncBody {
        shared: None,
        prepared: Default::default(),
        objects: Vec::new(),
        frame_size: 16,
        frame_align: 8,
        ret: ir::RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            ir::Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: ir::Operand::AddrImm(ir::LinkAddr(0x6a00_0000_2000)),
                    args: Vec::new(),
                    ret: ir::RetDest::Ignore,
                    target: 1,
                    unwind: ir::UnwindAction::Continue,
                    null_ok: false,
                    native_sig: Some(signature),
                },
            },
            ir::Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "indirect_native_probe".into(),
    }];
    let shared = Shared::new(ir::Module {
        funcs: funcs.into(),
        ..ir::Module::default()
    });
    let address = match &shared.module.funcs[0].blocks[0].term {
        Terminator::CallIndirect {
            native_sig: Some(sig),
            ..
        } => sig as *const ForeignSig as u64,
        other => unreachable!("the probe's terminator changed: {other:?}"),
    };

    let mut compiler = test_compiler(&shared, CodeDomain::Plain);
    compiler.reload = true;
    compiler.compile(0);
    let entry = compiler
        .last_entry
        .clone()
        .expect("the probe's body was captured");
    std::mem::forget(compiler);

    let baked = address.to_le_bytes();
    for symbol in &entry.symbols {
        assert!(
            !symbol.code.windows(8).any(|window| window == baked),
            "{} baked this process's signature address into the entry",
            symbol.name
        );
    }
    let site = Body::ForeignSig {
        block: 0,
        part: SigPart::Signature,
    };
    assert!(
        entry.symbols.iter().any(|symbol| symbol
            .relocs
            .iter()
            .any(|reloc| reloc.target == artifact::Target::Body(site))),
        "the entry does not name the signature site, so nothing can replay it"
    );
    // And the replay resolves that site to *this* process's signature: the address the writer knew,
    // recovered from the body rather than from the drawing process.
    let ordinals = artifact::Ordinals::of(&shared.module.funcs[0]).expect("the body numbers");
    assert_eq!(
        artifact::target_value(&shared, 0, &ordinals, &artifact::Target::Body(site)),
        Some(address),
        "the replayed signature address is not this process's"
    );
}

/// An entry whose frames this engine cannot describe is refused whole, not linked partially.
///
/// A stored entry names one CFA program per symbol and the loader turns them into FDEs at the
/// addresses the link placed the symbols at. An entry missing one — written by a build whose target
/// wanted another unwind kind, or damaged — would publish frames the unwinder walks past, which is a
/// wrong unwind rather than a missing one. So the link is refused and the session compiles instead.
#[test]
fn a_linked_entry_without_a_cfa_program_is_refused() {
    let shared = Shared::new(ir::Module {
        funcs: vec![body("frame_probe", Terminator::Return)].into(),
        ..ir::Module::default()
    });
    let mut compiler = test_compiler(&shared, CodeDomain::Plain);
    compiler.reload = true;
    compiler.compile(0);
    let captured = compiler.last_entry.clone().expect("the body was captured");
    std::mem::forget(compiler);

    // The entry as a store holds it: through the encoder and back.
    let entry = artifact::Entry::decode(&captured.encode().expect("the entry encodes"))
        .expect("the entry decodes");
    assert!(
        entry.symbols.iter().all(|symbol| symbol.unwind.is_some()),
        "a compiled entry must carry a CFA program per symbol, or this test proves nothing"
    );
    // The body polls signals at every block entry and that helper reads the calling thread's Engine
    // activation; the frame registration is what this test proves, so the poll resolves to a no-op.
    extern "C" fn probe_poll() {}
    let ordinals = artifact::Ordinals::of(&shared.module.funcs[0]).expect("the body has ordinals");
    let link = |entry: &artifact::Entry| {
        artifact::link(entry, &[JitSymbolRole::FastBody], |target| match target {
            artifact::Target::Named(name) if name.as_ref() == "mirvm_poll_signals" => {
                Some(probe_poll as *const u8 as u64)
            }
            artifact::Target::Named(name) => imports::whitelist()
                .get(name.as_ref())
                .map(|addr| *addr as u64),
            target => artifact::target_value(&shared, 0, &ordinals, target),
        })
        .expect("the probe body links")
    };

    let linked = link(&entry);
    assert!(
        Compiler::linked_frames(&linked, &entry).is_some(),
        "a complete entry was refused"
    );

    let mut stripped = entry.clone();
    stripped.symbols[0].unwind = None;
    let linked = link(&stripped);
    assert!(
        Compiler::linked_frames(&linked, &stripped).is_none(),
        "an entry without a CFA program still reported frames to register"
    );
}

#[test]
fn compiling_a_caller_keeps_callee_instructions_archived() {
    use crate::vm::archive::{bytes::Bytes, functions::Functions};
    use std::sync::Arc;

    for domain in [CodeDomain::Plain, CodeDomain::Trace] {
        for reload in [false, true] {
            let funcs: ir::FuncTable = vec![
                body(
                    "caller",
                    Terminator::Call {
                        callee: 1,
                        args: Vec::new(),
                        ret: RetDest::Ignore,
                        target: 1,
                        unwind: UnwindAction::Continue,
                        role: ir::CallRole::Normal,
                    },
                ),
                body("callee", Terminator::Return),
            ]
            .into();
            let bytes = Bytes::new(
                crate::vm::archive::functions::encode(&funcs)
                    .unwrap()
                    .to_vec(),
            );
            let len = bytes.len();
            let archive = Functions::load(bytes, 0..len).unwrap();
            let shared = Shared::new(ir::Module {
                funcs: ir::FuncTable::from_archive(Arc::new(archive)),
                ..Default::default()
            });
            let mut compiler = test_compiler(&shared, domain);
            compiler.reload = reload;
            assert_eq!(shared.module.funcs.decoded_count(), 0);
            compiler.compile(0);
            assert_ne!(
                shared.jit.slots_for(domain).slots[0].load(Ordering::Acquire),
                0
            );
            assert_ne!(
                shared.jit.slots_for(domain).slots_fast[1].load(Ordering::Acquire),
                0
            );
            assert_eq!(
                shared.module.funcs.decoded_count(),
                1,
                "{domain:?}, reload={reload}: preparing a callee entry decoded its instructions"
            );
        }
    }
}

#[test]
fn compiled_call_checks_the_callees_frame_before_entering_it() {
    // The caller fits in the synthetic headroom, but its callee does not. A missing call-site
    // check returns normally, while checking the caller's frame twice also misses the overflow.
    for reload in [false, true] {
        let caller = body(
            "small_caller",
            Terminator::Call {
                callee: 1,
                args: Vec::new(),
                ret: RetDest::Ignore,
                target: 1,
                unwind: UnwindAction::Continue,
                role: ir::CallRole::Normal,
            },
        );
        let mut callee = body("large_callee", Terminator::Return);
        callee.frame_size = 1 << 20;
        let mut module = ir::Module {
            funcs: vec![caller, callee].into(),
            ..Default::default()
        };
        module.exports.insert("caller".into(), 0);
        module.exports.insert("callee".into(), 1);
        let engine = crate::vm::ctx::Engine::new(Shared::new(module));
        let mut compiler = test_compiler(engine.shared(), CodeDomain::Plain);
        compiler.reload = reload;
        compiler.compile(1);
        compiler.compile(0);
        assert_ne!(engine.shared().jit.slots[0].load(Ordering::Acquire), 0);
        let activation = crate::vm::ctx::activate(engine.shared());
        let ctx = activation.ctx();
        let saved = unsafe { (*ctx).stack_floor };
        let marker = 0u8;
        unsafe {
            (*ctx).stack_floor = &marker as *const u8 as usize - STACK_GUARD_MARGIN - (64 << 10);
        }
        for entry in ["caller", "callee"] {
            let result = unsafe { crate::vm::interp::run_export(&engine, entry, &[]) };
            let error = result.expect_err("callee frame was entered without sufficient headroom");
            assert!(error.message.contains("guest stack overflow"), "{error:?}");
            assert!(error.message.contains("large_callee"), "{error:?}");
        }
        unsafe {
            (*ctx).stack_floor = saved;
        }
        drop(activation);
        std::mem::forget(compiler);
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn cleanup_call_artifacts_use_domain_slots_and_keep_unwind_metadata() {
    for domain in [CodeDomain::Plain, CodeDomain::Trace] {
        let mut caller = body(
            "cleanup_caller",
            Terminator::Call {
                callee: 1,
                args: Vec::new(),
                ret: RetDest::Ignore,
                target: 1,
                unwind: UnwindAction::Cleanup(2),
                role: ir::CallRole::Normal,
            },
        );
        caller.blocks.push(ir::Block {
            stmts: Vec::new(),
            term: Terminator::Resume,
        });
        let shared = Shared::new(ir::Module {
            funcs: vec![caller, body("callee", Terminator::Return)].into(),
            ..Default::default()
        });
        let mut compiler = test_compiler(&shared, domain);
        compiler.reload = true;
        compiler.compile(0);
        assert_ne!(
            shared.jit.slots_for(domain).slots[0].load(Ordering::Acquire),
            0
        );
        assert_ne!(
            shared.jit.slots_for(domain).slots_fast[1].load(Ordering::Acquire),
            0
        );
        let entry = compiler
            .last_entry
            .as_ref()
            .expect("captured cleanup caller");
        let fast = entry.symbol(JitSymbolRole::FastBody).unwrap();
        assert!(fast.lsda.is_some());
        assert!(fast.relocs.iter().any(|r| matches!(
            r.target,
            artifact::Target::Slot { domain: d, func: artifact::Ref::Ordinal(_) } if d == domain
        )));
        assert!(
            fast.relocs
                .iter()
                .any(|r| matches!(r.target, artifact::Target::StackNeed(_)))
        );
        assert!(!fast.relocs.iter().any(|r| matches!(
            &r.target,
            artifact::Target::Named(name) if name.as_ref() == "mirvm_c2i"
        )));
        std::mem::forget(compiler);
    }
}

#[test]
fn frame_objects_bound_escape_and_overlapping_scalar_views_stay_coherent() {
    let slot = |off, width| Slot { off, width };
    let place = |off| ir::PlaceExpr {
        base: ir::PlaceBase::Local(off),
        steps: Box::new([]),
    };
    let mut probe = body("escape", Terminator::Return);
    probe.frame_size = 40;
    probe.objects = vec![
        ir::FrameObject { off: 0, size: 16 },
        ir::FrameObject { off: 16, size: 24 },
    ];
    probe.blocks[0].stmts = vec![Stmt::Assign {
        dst: ScalarPlace::Slot(slot(24, Width::W64)),
        rv: ir::Rvalue::Ref(place(8)),
    }];
    let frame = analyze_frame(&probe);
    assert!(frame.contains(8));
    assert!(!frame.contains(16));
    assert!(!frame.contains(24));
    probe.objects.clear();
    assert!(analyze_frame(&probe).contains(24));

    probe.blocks[0].stmts = vec![
        Stmt::Assign {
            dst: ScalarPlace::Slot(slot(0, Width::W64)),
            rv: ir::Rvalue::Use(Operand::Imm {
                bits: 123,
                width: Width::W64,
            }),
        },
        Stmt::Assign {
            dst: ScalarPlace::Mem {
                expr: place(3),
                width: Width::W16,
            },
            rv: ir::Rvalue::Use(Operand::Imm {
                bits: 456,
                width: Width::W16,
            }),
        },
    ];
    let frame = analyze_frame(&probe);
    assert!(frame.contains(0));
    assert!(frame.contains(3));
}

#[test]
fn promoted_frame_copies_preserve_overlap_and_mixed_width_accesses() {
    for tier in [Tier::Baseline, Tier::Optimized] {
        for (src, dst, size) in [(0, 8, 24), (8, 0, 24), (0, 0, 32), (0, 32, 32)] {
            for mixed in [false, true] {
                let slot = |off| Slot {
                    off,
                    width: Width::W64,
                };
                let place = |off| ir::PlaceExpr {
                    base: ir::PlaceBase::Local(off),
                    steps: Box::new([]),
                };
                let mut probe = body("copy", Terminator::Return);
                probe.frame_size = 64;
                probe.ret = RetAbi::Pair(slot(dst), slot(dst + size - 8));
                for off in (0..64).step_by(8) {
                    probe.blocks[0].stmts.push(Stmt::Assign {
                        dst: ScalarPlace::Slot(slot(off)),
                        rv: ir::Rvalue::Use(Operand::Imm {
                            bits: u64::from(off) + 100,
                            width: Width::W64,
                        }),
                    });
                }
                if mixed {
                    probe.blocks[0].stmts.push(Stmt::Assign {
                        dst: ScalarPlace::Mem {
                            expr: place(src + 1),
                            width: Width::W16,
                        },
                        rv: ir::Rvalue::Use(Operand::Imm {
                            bits: 0x1234,
                            width: Width::W16,
                        }),
                    });
                }
                probe.blocks[0].stmts.push(Stmt::Copy {
                    dst: place(dst),
                    src: place(src),
                    size,
                });
                assert_eq!(analyze_frame(&probe).needs_frame(), mixed);
                let mut module = ir::Module {
                    funcs: vec![probe].into(),
                    ..Default::default()
                };
                module.exports.insert("copy".into(), 0);
                let engine = crate::vm::ctx::Engine::new(Shared::new(module));
                let expected =
                    unsafe { crate::vm::interp::run_export(&engine, "copy", &[]) }.unwrap();
                let mut compiler = Compiler::with_tier(engine.shared(), CodeDomain::Plain, tier);
                compiler.jit_cache = false;
                compiler.reload = true;
                compiler.compile(0);
                assert_ne!(engine.shared().jit.slots[0].load(Ordering::Acquire), 0);
                assert_eq!(
                    unsafe { crate::vm::interp::run_export(&engine, "copy", &[]) }.unwrap(),
                    expected
                );
                std::mem::forget(compiler);
            }
        }
    }
}
#[test]
fn leaf_inlining_uses_private_ssa_and_keys_the_bound_callee_content() {
    let slot = |off| Slot {
        off,
        width: Width::W64,
    };
    let mut caller = body(
        "caller",
        Terminator::Call {
            callee: 1,
            args: vec![Operand::Slot(slot(0))],
            ret: RetDest::Scalar(ScalarPlace::Slot(slot(8))),
            target: 1,
            unwind: UnwindAction::Continue,
            role: ir::CallRole::Normal,
        },
    );
    caller.frame_size = 16;
    caller.params = vec![ParamAbi::Scalar(slot(0))];
    caller.ret = RetAbi::Pair(slot(0), slot(8));
    let mut keys = Vec::new();
    for value in [7, 17] {
        let mut leaf = body("leaf", Terminator::Return);
        leaf.blocks.truncate(1);
        leaf.frame_size = 16;
        leaf.params = vec![ParamAbi::Scalar(slot(0))];
        leaf.ret = RetAbi::Scalar(slot(8));
        leaf.blocks[0].stmts = vec![Stmt::Assign {
            dst: ScalarPlace::Slot(slot(8)),
            rv: ir::Rvalue::IntBin {
                op: IntBinOp::Add,
                signed: false,
                a: Operand::Slot(slot(0)),
                b: Operand::Imm {
                    bits: value,
                    width: Width::W64,
                },
            },
        }];
        let mut module = ir::Module {
            funcs: vec![caller.clone(), leaf].into(),
            ..Default::default()
        };
        module.exports.insert("caller".into(), 0);
        let engine = crate::vm::ctx::Engine::new(Shared::new(module));
        let expected = unsafe { crate::vm::interp::run_export(&engine, "caller", &[100]) }.unwrap();
        let mut compiler = test_compiler(engine.shared(), CodeDomain::Plain);
        compiler.reload = true;
        compiler.compile(0);
        let entry = compiler.last_entry.as_ref().unwrap();
        assert_eq!(entry.jit.inlined.len(), 1);
        let plan = super::super::inline::Plan::for_body(
            engine.shared(),
            &engine.shared().module.funcs[0],
            true,
        );
        assert_eq!(entry.jit.inlined, plan.dependencies);
        keys.push(entry.jit.digest());
        assert_eq!(
            unsafe { crate::vm::interp::run_export(&engine, "caller", &[100]) }.unwrap(),
            expected
        );
        std::mem::forget(compiler);
    }
    assert_ne!(keys[0], keys[1]);
}

#[test]
fn baseline_folding_preserves_branch_merges_and_narrow_signed_values() {
    let slot = |off, width| Slot { off, width };
    let input = slot(0, Width::W64);
    let local = slot(8, Width::W8);
    let output = slot(16, Width::W64);
    let assign = |dst, rv| Stmt::Assign {
        dst: ScalarPlace::Slot(dst),
        rv,
    };
    let mut probe = body(
        "baseline_join",
        Terminator::SwitchInt {
            discr: ir::SwitchDiscr::Scalar(Operand::Slot(input)),
            targets: vec![(0, 1)],
            otherwise: 2,
        },
    );
    probe.frame_size = 24;
    probe.params = vec![ParamAbi::Scalar(input)];
    probe.ret = RetAbi::Scalar(output);
    probe.blocks.truncate(1);
    for bits in [255, 127] {
        probe.blocks.push(ir::Block {
            stmts: vec![
                assign(
                    local,
                    ir::Rvalue::Use(Operand::Imm {
                        bits,
                        width: Width::W8,
                    }),
                ),
                assign(
                    local,
                    ir::Rvalue::IntBin {
                        op: IntBinOp::Add,
                        signed: true,
                        a: Operand::Slot(local),
                        b: Operand::Imm {
                            bits: 1,
                            width: Width::W8,
                        },
                    },
                ),
            ],
            term: Terminator::Goto(3),
        });
    }
    probe.blocks.push(ir::Block {
        stmts: vec![assign(
            output,
            ir::Rvalue::Cast {
                from: (Width::W8, true),
                to: Width::W64,
                a: Operand::Slot(local),
            },
        )],
        term: Terminator::Return,
    });
    let mut module = ir::Module {
        funcs: vec![probe].into(),
        ..Default::default()
    };
    module.exports.insert("baseline_join".into(), 0);
    let engine = crate::vm::ctx::Engine::new(Shared::new(module));
    let expected: Vec<_> = [0, 1]
        .into_iter()
        .map(|arg| unsafe {
            crate::vm::interp::run_export(&engine, "baseline_join", &[arg]).unwrap()
        })
        .collect();
    let mut compiler = Compiler::with_tier(engine.shared(), CodeDomain::Plain, Tier::Baseline);
    compiler.jit_cache = false;
    compiler.reload = true;
    compiler.compile(0);
    assert_ne!(engine.shared().jit.slots[0].load(Ordering::Acquire), 0);
    for (arg, expected) in [0, 1].into_iter().zip(expected) {
        assert_eq!(
            unsafe { crate::vm::interp::run_export(&engine, "baseline_join", &[arg]) }.unwrap(),
            expected
        );
    }
    std::mem::forget(compiler);
}

#[test]
fn cheap_entry_tier_requires_bounded_straight_line_work() {
    let mut probe = body("tier", Terminator::Return);
    assert_eq!(
        isa::entry_tier(ir::FuncHeader::Owned(&probe), Tier::Baseline),
        Tier::Optimized
    );
    probe.blocks.truncate(1);
    assert_eq!(
        isa::entry_tier(ir::FuncHeader::Owned(&probe), Tier::Baseline),
        Tier::Baseline
    );
    assert_eq!(
        isa::entry_tier(ir::FuncHeader::Owned(&probe), Tier::Optimized),
        Tier::Optimized
    );
    probe.blocks[0].term = Terminator::Goto(0);
    assert_eq!(
        isa::entry_tier(ir::FuncHeader::Owned(&probe), Tier::Baseline),
        Tier::Optimized
    );
    probe.blocks[0].term = Terminator::Return;
    probe.frame_size = 512;
    assert_eq!(
        isa::entry_tier(ir::FuncHeader::Owned(&probe), Tier::Baseline),
        Tier::Optimized
    );
}
