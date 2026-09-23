//! Engine faults: how a VM invariant violation crosses Engine boundaries.

use super::*;

static FOREIGN_FAULT_CATCH_RAN: AtomicU64 = AtomicU64::new(0);

static FOREIGN_FAULT_CLEANUP_RAN: AtomicU64 = AtomicU64::new(0);

static FOREIGN_FAULT_BOUNDARY_RESULT: AtomicU64 = AtomicU64::new(0);

static SUSPENDED_FAULT_GUEST_CLEANUP_RAN: AtomicU64 = AtomicU64::new(0);

unsafe extern "C-unwind" fn host_panic_entry() {
    std::panic::panic_any(HostPanicMarker(0xe13));
}

unsafe extern "C-unwind" fn nested_engine_entry() {
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("nested Engine was not installed")
            .clone()
    });
    let observed = match unsafe { run_export(&engine, "probe", &[]) } {
        Err(error) if error.kind == RunErrorKind::EngineFault => 1,
        Err(_) => 2,
        Ok(_) => 3,
    };
    // A correctly owned EngineFault never returns to this callback: it must
    // pass through B's boundary and continue to A's outer boundary.
    FOREIGN_FAULT_BOUNDARY_RESULT.store(observed, Ordering::SeqCst);
}

fn guest_panic_with_cleanup_function() -> FuncBody {
    FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::UnwindRaise,
                    args: vec![Operand::Imm {
                        bits: 0x5151,
                        width: Width::W64,
                    }],
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Cleanup(2),
                    role: crate::vm::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
            Block {
                stmts: vec![marker_store(&SUSPENDED_FAULT_GUEST_CLEANUP_RAN)],
                term: Terminator::Resume,
            },
        ],
        name: "guest_panic_while_engine_fault_is_suspended".into(),
    }
}

fn owner_fault_roundtrip_module() -> Module {
    let mut module = Module {
        funcs: vec![
            calls_native("owner_calls_nested_engine", nested_engine_entry),
            engine_fault_function("owner_engine_fault", 0),
            function(
                "owner_recovers_after_fault",
                0,
                RetAbi::Zst,
                Terminator::Return,
            ),
        ]
        .into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.exports.insert("recover".into(), 2);
    module
}

fn foreign_fault_traversal_module(owner_fault_thunk: u64) -> Module {
    let result = word(0);
    let try_addr = 0xc001;
    let catch_addr = 0xc002;
    let outer = FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Scalar(result),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::CatchUnwind,
                    args: vec![
                        Operand::Imm {
                            bits: try_addr,
                            width: Width::W64,
                        },
                        Operand::Imm {
                            bits: 0,
                            width: Width::W64,
                        },
                        Operand::Imm {
                            bits: catch_addr,
                            width: Width::W64,
                        },
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(result)),
                    target: 1,
                    unwind: UnwindAction::Cleanup(2),
                    role: crate::vm::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
            Block {
                stmts: vec![marker_store(&FOREIGN_FAULT_CLEANUP_RAN)],
                term: Terminator::Resume,
            },
        ],
        name: "foreign_fault_catch_frame".into(),
    };
    let try_body = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(word(8))],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: owner_fault_thunk,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Cleanup(2),
                    null_ok: false,
                    native_sig: Some(callback_sig()),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
            Block {
                stmts: vec![marker_store(&FOREIGN_FAULT_CLEANUP_RAN)],
                term: Terminator::Resume,
            },
        ],
        name: "foreign_fault_try_frame".into(),
    };
    let mut catch_body = function(
        "foreign_fault_must_not_reach_guest_catch",
        2,
        RetAbi::Zst,
        Terminator::Return,
    );
    catch_body.blocks[0]
        .stmts
        .push(marker_store(&FOREIGN_FAULT_CATCH_RAN));

    let mut module = Module {
        funcs: vec![outer, try_body, catch_body].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_entry_links.push((LinkAddr(try_addr), 1));
    module.fn_entry_links.push((LinkAddr(catch_addr), 2));
    module
}

#[test]
fn engine_fault_crosses_foreign_engine_and_is_finished_by_its_owner() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        FOREIGN_FAULT_CATCH_RAN.store(0, Ordering::SeqCst);
        FOREIGN_FAULT_CLEANUP_RAN.store(0, Ordering::SeqCst);
        FOREIGN_FAULT_BOUNDARY_RESULT.store(0, Ordering::SeqCst);

        let owner = engine(owner_fault_roundtrip_module(), jit);
        let owner_fault_thunk = thunks::get_or_create(owner.shared(), 0xd001, 1, &callback_sig());
        let foreign = engine(foreign_fault_traversal_module(owner_fault_thunk), jit);

        let fault_result =
            with_nested_engine(&foreign, || unsafe { run_export(&owner, "probe", &[]) });
        let boundary_result = FOREIGN_FAULT_BOUNDARY_RESULT.load(Ordering::SeqCst);
        let catch_ran = FOREIGN_FAULT_CATCH_RAN.load(Ordering::SeqCst);
        let cleanup_ran = FOREIGN_FAULT_CLEANUP_RAN.load(Ordering::SeqCst);
        let fault_still_in_flight = super::super::ctx::engine_fault_in_flight();
        let recovery = unsafe { run_export(&owner, "recover", &[]) };

        let owner_classified = matches!(
            &fault_result,
            Err(error)
                if error.kind == RunErrorKind::EngineFault
                    && error.exit_code == 70
                    && error.message.contains("embedding contract engine fault")
        );
        let owner_recovered = matches!(recovery, Ok(RunOutcome::Returned(value)) if value.lo == 0);
        if !owner_classified
            || boundary_result != 0
            || catch_ran != 0
            || cleanup_ran != 0
            || fault_still_in_flight
            || !owner_recovered
        {
            failures.push(format!(
                "{mode}: fault={fault_result:?}, B-boundary={boundary_result}, \
                 B-catch={catch_ran}, B-cleanup={cleanup_ran}, \
                 in-flight={fault_still_in_flight}, recovery={recovery:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "EngineFault changed owner or ran guest cleanup while crossing another Engine: {failures:#?}"
    );
}

#[test]
fn suspended_engine_fault_does_not_hide_reentrant_guest_panic_cleanup() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        SUSPENDED_FAULT_GUEST_CLEANUP_RAN.store(0, Ordering::SeqCst);

        let fault_owner = engine(Module::default(), false);
        let activation = super::super::ctx::activate(fault_owner.shared());
        let suspended = unwind::catch_raw(|| {
            unwind::raise_engine_fault(activation.ctx(), "suspended by a native catch".into(), 70)
        })
        .expect_err("EngineFault must reach the native catch");

        let reentrant = engine(export_module(guest_panic_with_cleanup_function()), jit);
        let outcome = unsafe { run_export(&reentrant, "probe", &[]) };
        let cleanup_ran = SUSPENDED_FAULT_GUEST_CLEANUP_RAN.load(Ordering::SeqCst);

        let payload = suspended
            .take_mirvm(fault_owner.shared())
            .expect("the suspended EngineFault must retain its owner");
        let unwind::MirvmPayload::EngineFault(fault) = payload else {
            panic!("suspended EngineFault changed kind")
        };
        let report = fault.finish();

        if !matches!(outcome, Ok(RunOutcome::GuestPanic))
            || cleanup_ran != 1
            || report.message != "suspended by a native catch"
            || super::super::ctx::engine_fault_in_flight()
        {
            failures.push(format!(
                "{mode}: nested={outcome:?}, cleanup={cleanup_ran}, report={report:?}, in-flight={}",
                super::super::ctx::engine_fault_in_flight()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "a suspended EngineFault hid cleanup for a distinct reentrant guest panic: {failures:#?}"
    );
}

#[test]
fn suspended_engine_fault_allows_a_reentrant_engine_fault() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let outer_owner = engine(Module::default(), false);
        let activation = super::super::ctx::activate(outer_owner.shared());
        let suspended = unwind::catch_raw(|| {
            unwind::raise_engine_fault(activation.ctx(), "outer suspended EngineFault".into(), 70)
        })
        .expect_err("outer EngineFault must reach the native catch");

        let reentrant = engine(
            export_module(engine_fault_function("reentrant_engine_fault", 0)),
            jit,
        );
        let inner = unsafe { run_export(&reentrant, "probe", &[]) };
        let outer_remained_live = super::super::ctx::engine_fault_in_flight();

        let payload = suspended
            .take_mirvm(outer_owner.shared())
            .expect("the outer EngineFault must retain its owner");
        let unwind::MirvmPayload::EngineFault(fault) = payload else {
            panic!("outer EngineFault changed kind")
        };
        let report = fault.finish();

        if !matches!(
            inner,
            Err(ref error)
                if error.kind == RunErrorKind::EngineFault
                    && error.message.contains("embedding contract engine fault")
        ) || !outer_remained_live
            || report.message != "outer suspended EngineFault"
            || super::super::ctx::engine_fault_in_flight()
        {
            failures.push(format!(
                "{mode}: inner={inner:?}, outer-live={outer_remained_live}, report={report:?}, in-flight={}",
                super::super::ctx::engine_fault_in_flight()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "a suspended EngineFault prevented an independent reentrant EngineFault: {failures:#?}"
    );
}

#[test]
fn engine_top_rethrows_host_rust_panic_unchanged() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let host = engine(
            export_module(calls_native("host_panic_export", host_panic_entry)),
            jit,
        );
        let caught = catch_unwind(AssertUnwindSafe(|| unsafe {
            run_export(&host, "probe", &[])
        }));
        match caught {
            Err(payload)
                if payload.downcast_ref::<HostPanicMarker>() == Some(&HostPanicMarker(0xe13)) => {}
            Err(_) => failures.push(format!("{mode}: host panic payload type or value changed")),
            Ok(result) => failures.push(format!(
                "{mode}: Engine boundary consumed host panic as {result:?}"
            )),
        }
        if super::super::ctx::engine_fault_in_flight() {
            failures.push(format!(
                "{mode}: host panic incorrectly left EngineFault state active"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Engine top did not preserve a host Rust panic: {failures:#?}"
    );
}

#[test]
fn interpreter_prologue_fault_restores_frame_state() {
    let broken = engine(
        export_module(function(
            "missing_required_argument",
            1,
            RetAbi::Zst,
            Terminator::Return,
        )),
        false,
    );
    let activation = super::super::ctx::activate(broken.shared());
    let ctx = activation.ctx();
    assert_eq!(unsafe { (*ctx).depth }, 0);
    assert_eq!(unsafe { (*ctx).region.used() }, 0);

    let error = unsafe { run_export(&broken, "probe", &[]) }
        .expect_err("missing argument must be an EngineFault");
    assert_eq!(error.kind, RunErrorKind::EngineFault);
    assert_eq!(unsafe { (*ctx).depth }, 0);
    assert_eq!(unsafe { (*ctx).region.used() }, 0);
    assert!(unsafe { (*ctx).shadow.is_empty() });

    let outcome = unsafe { run_export(&broken, "probe", &[7]) }.unwrap();
    assert_eq!(outcome, RunOutcome::Returned(Default::default()));
    assert_eq!(unsafe { (*ctx).depth }, 0);
    assert_eq!(unsafe { (*ctx).region.used() }, 0);
    assert!(unsafe { (*ctx).shadow.is_empty() });
}
