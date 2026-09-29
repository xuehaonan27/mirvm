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

/// The trace domain is a separate ISA because pinning a register is an ISA-wide
/// decision. Plain code must keep the pinned register allocatable and carry no recorder state,
/// so the two domains cannot share one `Flags`.
#[test]
fn trace_domain_enables_the_pinned_register_and_plain_does_not() {
    let trace = domain_flags(Tier::Optimized, true);
    assert!(
        trace.enable_pinned_reg(),
        "trace domain must enable the pinned register ({})",
        crate::arch::PINNED_REG
    );

    // The plain domain must stay exactly as it is: no pinned register, so
    // plain code keeps r15 free and costs nothing for collection.
    let mut plain_builder = settings::builder();
    plain_builder.set("opt_level", "speed").unwrap();
    plain_builder.set("unwind_info", "true").unwrap();
    plain_builder
        .set("preserve_frame_pointers", "true")
        .unwrap();
    let plain = settings::Flags::new(plain_builder);
    assert!(
        !plain.enable_pinned_reg(),
        "plain domain must not reserve a register for collection"
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

/// The trace domain's boundary is the one place the pinned register is installed
/// and the one place it is restored. A body that reads the pin
/// proves the install; reading the *host's* register before and after a call
/// proves the restore, on the normal path and on the unwinding path alike --
/// the latter is why the boundary carries an LSDA instead of being a plain
/// save/call/restore sequence.
#[test]
fn trace_entry_pins_the_recorder_and_restores_it_even_when_it_unwinds() {
    const PINNED: u64 = 0x5052_4f44_5543_4552;
    let shared = Shared::new(ir::Module {
        funcs: vec![body("domain_body", Terminator::Return)].into(),
        ..ir::Module::default()
    });
    let mut compiler = test_compiler(&shared, CodeDomain::Trace);
    let enter = shared.jit.trace_enter.load(Ordering::Acquire);
    assert_ne!(enter, 0, "the trace domain published no boundary entry");

    let probe = define_pin_probe(&mut compiler);
    type Packed = extern "C" fn(*const u64, *mut u64);
    let read_pin: Packed = unsafe { std::mem::transmute(probe as usize) };

    let mut ret = [0u64; 2];
    // Read the host's own register first: everything below must leave it
    // exactly like this.
    let mut host = [0u64; 2];
    read_pin(std::ptr::null(), host.as_mut_ptr());
    let host_value = std::hint::black_box(host[0]);
    assert_ne!(
        host_value, PINNED,
        "the host already held the probe value, so this test proves nothing"
    );

    // Called through the boundary, the probe sees the recorder the boundary
    // installed. It only reads, so the host's own register survives.
    unsafe { crate::vm::jit::call_trace_body(enter, PINNED as *mut _, probe, &[], &mut ret) };
    assert_eq!(
        ret[0], PINNED,
        "the boundary did not pin the recorder register before the call"
    );

    unsafe { crate::vm::jit::call_trace_body(enter, PINNED as *mut _, probe, &[], &mut ret) };
    read_pin(std::ptr::null(), host.as_mut_ptr());
    assert_eq!(
        std::hint::black_box(host[0]),
        host_value,
        "the normal path did not restore the host's register"
    );

    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        crate::vm::jit::call_trace_body(
            enter,
            PINNED as *mut _,
            unwind_through_boundary as *const u8 as u64,
            &[],
            &mut ret,
        )
    }));
    assert!(
        caught.is_err(),
        "a panic raised inside a trace body did not reach the host"
    );
    read_pin(std::ptr::null(), host.as_mut_ptr());
    let after_unwind = std::hint::black_box(host[0]);
    assert_ne!(
        after_unwind, PINNED,
        "the unwinding path left the recorder pinned in the host's register"
    );
    assert_eq!(
        after_unwind, host_value,
        "the unwinding path did not restore the host's register"
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

fn body(name: &str, first: Terminator) -> ir::FuncBody {
    ir::FuncBody {
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
fn real_compilation_registers_every_executable_role() {
    let caller = body(
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
    let callee = body("profile_callee", Terminator::Return);
    let shared = Shared::new(ir::Module {
        funcs: vec![caller, callee].into(),
        ..ir::Module::default()
    });
    let mut compiler = test_compiler(&shared, CodeDomain::Plain);

    compiler.compile(0);

    assert_ne!(shared.jit.slots[0].load(Ordering::Acquire), 0);
    let ranges = shared.jit.symbol_ranges();
    assert_eq!(ranges.len(), 4);
    assert!(ranges.iter().any(|range| {
        range.func_id == 0 && range.role == JitSymbolRole::FastBody && range.size != 0
    }));
    assert!(ranges.iter().any(|range| {
        range.func_id == 0 && range.role == JitSymbolRole::Guarded && range.size != 0
    }));
    assert!(ranges.iter().any(|range| {
        range.func_id == 0 && range.role == JitSymbolRole::Packed && range.size != 0
    }));
    assert!(ranges.iter().any(|range| {
        range.func_id == 1 && range.role == JitSymbolRole::C2i && range.size != 0
    }));
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
    let shared = Shared::new(ir::Module {
        funcs: vec![
            body("failed_profile_target", Terminator::Return),
            body("successful_profile_target", Terminator::Return),
        ]
        .into(),
        ..ir::Module::default()
    });
    let mut compiler = test_compiler(&shared, CodeDomain::Plain);
    compiler.fail_after_symbol = Some(JitSymbolRole::FastBody);

    compiler.compile(0);

    assert_eq!(shared.jit.slots[0].load(Ordering::Acquire), 0);
    assert!(shared.jit.symbol_ranges().is_empty());

    compiler.fail_after_symbol = None;
    compiler.compile(1);

    assert_ne!(shared.jit.slots[1].load(Ordering::Acquire), 0);
    let ranges = shared.jit.symbol_ranges();
    assert_eq!(ranges.len(), 3);
    assert!(ranges.iter().all(|range| range.func_id == 1));
    assert!(
        ranges
            .iter()
            .any(|range| range.role == JitSymbolRole::FastBody)
    );
    assert!(
        ranges
            .iter()
            .any(|range| range.role == JitSymbolRole::Guarded)
    );
    assert!(
        ranges
            .iter()
            .any(|range| range.role == JitSymbolRole::Packed)
    );
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
        let module_entry = match compiler.module.get_name("g0") {
            Some(cranelift_module::FuncOrDataId::Func(id)) => {
                compiler.module.get_finalized_function(id) as u64
            }
            other => panic!("the module has no guarded entry for f0: {other:?}"),
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
    /// The probe calls its callee through the PLT slot and reads a frozen link address: one fragment
    /// reference of each kind this test needs, in the canonical order the walk gives them.
    fn probe(frozen: u64, callee: u32) -> ir::FuncBody {
        let slot = || ir::Slot {
            off: 0,
            width: ir::Width::W64,
        };
        ir::FuncBody {
            frame_size: 16,
            frame_align: 8,
            ret: ir::RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![
                ir::Block {
                    stmts: vec![ir::Stmt::Assign {
                        dst: ir::ScalarPlace::Slot(slot()),
                        rv: ir::Rvalue::Use(ir::Operand::AddrImm(ir::LinkAddr(frozen))),
                    }],
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
    fn entry_of(funcs: Vec<ir::FuncBody>, which: u32) -> artifact::Entry {
        let shared = Shared::new(ir::Module {
            funcs: funcs.into(),
            ..ir::Module::default()
        });
        let mut compiler = test_compiler(&shared, CodeDomain::Plain);
        compiler.reload = true;
        compiler.compile(which);
        let entry = compiler
            .last_entry
            .clone()
            .expect("the function was captured");
        std::mem::forget(compiler);
        entry
    }

    let callee = || body("artifact_callee", Terminator::Return);
    // The same fragment at function 0 with its callee at 1, and at function 1 with its callee at 0;
    // the frozen address differs too, because a program places its data where it likes.
    let first = entry_of(vec![probe(0x6a00_0000_1000, 1), callee()], 0);
    let second = entry_of(vec![callee(), probe(0x6a00_0000_9000, 0)], 1);

    assert!(
        first.symbol(JitSymbolRole::FastBody).is_some(),
        "the compared entries hold the body, or the comparison proves nothing"
    );
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
