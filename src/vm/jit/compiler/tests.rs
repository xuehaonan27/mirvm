//! Standing checks for the compilation pipeline: domain isolation, the trace boundary
//! pin/restore contract, and symbol registration.

use super::*;

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
        let mut compiler = Compiler::with_domain(&shared, domain);
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
    let trace = trace_domain_flags();
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
    let mut compiler = Compiler::with_domain(&shared, CodeDomain::Trace);
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
                bits: libc::SYS_getpid as u64,
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
    let mut compiler = Compiler::with_domain(&shared, CodeDomain::Trace);
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
    let mut compiler = Compiler::with_domain(&shared, CodeDomain::Plain);

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
    let mut compiler = Compiler::with_domain(&shared, CodeDomain::Plain);
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
