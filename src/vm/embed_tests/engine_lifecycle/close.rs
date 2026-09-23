//! Close ordering: active calls, the seal and the EngineClosed boundary.

use super::*;

static FOREIGN_CLOSED_CLEANUP_RAN: AtomicU64 = AtomicU64::new(0);

fn p1_identity_module(link_addr: LinkAddr, unwind: bool) -> Module {
    let ret = word(0);
    let mut body = function("p1_identity", 0, RetAbi::Scalar(ret), Terminator::Return);
    body.blocks[0].stmts.push(Stmt::Assign {
        dst: ScalarPlace::Slot(ret),
        rv: Rvalue::Use(Operand::Imm {
            bits: 73,
            width: Width::W64,
        }),
    });
    let sig = ForeignSig {
        args: Vec::new(),
        ret: FfiKind::U64,
        fixed: None,
        thunk_args: Vec::new(),
        unwind,
    };
    let mut module = Module {
        funcs: vec![body].into(),
        ..Module::default()
    };
    module.fn_entry_links.push((link_addr, 0));
    module
        .entry_stub_sites
        .push(super::super::ir::EntryStubSite {
            link_addr,
            func: 0,
            sig,
        });
    module
}

fn foreign_closed_traversal_module(owner_thunk: u64) -> Module {
    let mut module = Module {
        funcs: vec![FuncBody {
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallIndirect {
                        callee: Operand::Imm {
                            bits: owner_thunk,
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
                    stmts: vec![marker_store(&FOREIGN_CLOSED_CLEANUP_RAN)],
                    term: Terminator::Resume,
                },
            ],
            name: "foreign_engine_closed_cleanup_frame".into(),
        }]
        .into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module
}

#[test]
fn close_waits_for_a_real_active_call_before_teardown() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        reset_lifecycle_gate();
        let engine = engine(
            export_module(calls_native(
                "lifecycle_blocking_export",
                lifecycle_blocking_entry,
            )),
            jit,
        );
        let id = engine.shared().id;
        let execution = engine.clone();
        let running = std::thread::spawn(move || unsafe { run_export(&execution, "probe", &[]) });
        wait_lifecycle_entry();

        engine.close();
        let state_while_active = engine.state();
        let registry_while_active = super::super::ctx::engine(id).is_some();
        let rejected = unsafe { run_export(&engine, "probe", &[]) };
        release_lifecycle_entry();
        let completed = running.join().expect("active guest call thread panicked");
        engine.wait_closed().unwrap();

        if state_while_active != super::super::ctx::EngineState::Closing
            || !registry_while_active
            || !matches!(
                rejected,
                Err(ref error) if error.kind == RunErrorKind::EngineClosed
            )
            || !matches!(completed, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || engine.state() != super::super::ctx::EngineState::Closed
            || super::super::ctx::engine(id).is_some()
        {
            failures.push(format!(
                "{mode}: active-state={state_while_active:?}, registry={registry_while_active}, \
                 rejected={rejected:?}, completed={completed:?}, final={:?}",
                engine.state()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Engine close tore down active execution or admitted new work: {failures:#?}"
    );
}

#[test]
fn suspended_guest_exception_keeps_engine_closing_until_consumed() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let owner = engine(
            Module {
                funcs: vec![guest_panic_function("suspended_guest_panic", 0)].into(),
                ..Module::default()
            },
            jit,
        );
        let code = thunks::get_or_create(owner.shared(), 0xe225, 0, &callback_sig());
        let callback: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(code as usize) };
        let exception = unwind::catch_raw(|| unsafe { callback() })
            .expect_err("guest panic must be suspended outside the callback thunk");

        owner.close();
        let state_while_suspended = owner.state();

        let payload = exception
            .take_mirvm(owner.shared())
            .expect("suspended guest panic changed owner");
        let unwind::MirvmPayload::Guest(payload) = payload else {
            panic!("suspended guest panic changed kind")
        };
        let inner = payload.transfer(|_, inner| inner);
        owner.wait_closed().unwrap();

        if state_while_suspended != super::super::ctx::EngineState::Closing
            || owner.state() != super::super::ctx::EngineState::Closed
            || inner != 0x4747
        {
            failures.push(format!(
                "{mode}: suspended={state_while_suspended:?}, final={:?}, inner={inner:#x}",
                owner.state()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Engine finalized while a native catch retained its guest exception: {failures:#?}"
    );
}

#[test]
fn wait_closed_from_own_native_callback_fails_fast() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        LIFECYCLE_WAIT_RESULT.store(0, Ordering::SeqCst);
        let engine = engine(
            export_module(calls_native(
                "wait_closed_from_native_callback",
                lifecycle_wait_from_callback,
            )),
            jit,
        );

        let result = with_nested_engine(&engine, || unsafe { run_export(&engine, "probe", &[]) });
        let outer_wait = engine.wait_closed();
        if !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || LIFECYCLE_WAIT_RESULT.load(Ordering::SeqCst) != 1
            || outer_wait.is_err()
            || engine.state() != super::super::ctx::EngineState::Closed
        {
            failures.push(format!(
                "{mode}: result={result:?}, callback-wait={}, outer-wait={outer_wait:?}, state={:?}",
                LIFECYCLE_WAIT_RESULT.load(Ordering::SeqCst),
                engine.state()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "wait_closed blocked on its own callback lease: {failures:#?}"
    );
}

#[test]
fn closed_c_unwind_thunk_raises_structured_engine_closed() {
    let engine = engine(
        Module {
            funcs: vec![function(
                "closed_thunk_target",
                0,
                RetAbi::Zst,
                Terminator::Return,
            )]
            .into(),
            ..Module::default()
        },
        false,
    );
    let control = std::sync::Arc::clone(engine.control());
    let code = thunks::get_or_create(engine.shared(), 0xe220, 0, &callback_sig());
    engine.wait_closed().unwrap();

    let callback: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(code as usize) };
    let exception = unwind::catch_raw(|| unsafe { callback() })
        .expect_err("closed C-unwind thunk must raise EngineClosed");
    exception
        .take_engine_closed(&control)
        .expect("closed thunk raised the wrong exception kind or owner");
}

#[test]
fn p1_entries_are_per_engine_and_never_aba_after_close() {
    let link_addr = LinkAddr(0x6b00_0000_0100);
    let first = engine(p1_identity_module(link_addr, true), false);
    let second = engine(p1_identity_module(link_addr, true), false);
    let first_control = std::sync::Arc::clone(first.control());
    let first_addr = first.shared().instance.resolve_link_addr(link_addr);
    let second_addr = second.shared().instance.resolve_link_addr(link_addr);
    assert_ne!(
        first_addr, second_addr,
        "each Engine must own a distinct P1 closure"
    );

    let first_call: unsafe extern "C-unwind" fn() -> u64 =
        unsafe { std::mem::transmute(first_addr as usize) };
    let second_call: unsafe extern "C-unwind" fn() -> u64 =
        unsafe { std::mem::transmute(second_addr as usize) };
    assert_eq!(unsafe { first_call() }, 73);
    assert_eq!(unsafe { second_call() }, 73);

    first.wait_closed().unwrap();
    unwind::catch_raw(|| unsafe { first_call() })
        .expect_err("old P1 pointer must report its closed owner")
        .take_engine_closed(&first_control)
        .expect("old P1 pointer changed owner after close");
    assert_eq!(unsafe { second_call() }, 73, "closing A must not affect B");

    let third = engine(p1_identity_module(link_addr, true), false);
    let third_addr = third.shared().instance.resolve_link_addr(link_addr);
    assert_ne!(
        third_addr, first_addr,
        "P1 closure addresses must never be reused"
    );
    assert_ne!(third_addr, second_addr);
    unwind::catch_raw(|| unsafe { first_call() })
        .expect_err("old P1 pointer must stay a tombstone after a new Engine opens")
        .take_engine_closed(&first_control)
        .expect("old P1 pointer ABA-routed into the new Engine");
}

#[test]
fn closed_plain_c_p1_entry_aborts() {
    const CHILD: &str = "MIRVM_CLOSED_P1_C_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let link_addr = LinkAddr(0x6b00_0000_0200);
        let engine = engine(p1_identity_module(link_addr, false), false);
        let addr = engine.shared().instance.resolve_link_addr(link_addr);
        engine.wait_closed().unwrap();
        let callback: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(addr as usize) };
        let _ = unsafe { callback() };
        panic!("closed plain C P1 entry returned");
    }

    let test_name = "vm::embed_tests::engine_lifecycle::close::closed_plain_c_p1_entry_aborts";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("failed to start closed plain C P1 subprocess");
    assert!(
        !output.status.success(),
        "closed plain C P1 returned normally"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("plain C thunk called after its Engine was closed"),
        "closed plain C P1 failed for the wrong reason:\n{stderr}"
    );
}

#[test]
fn engine_closed_runs_cleanup_while_crossing_another_engine() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        FOREIGN_CLOSED_CLEANUP_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            Module {
                funcs: vec![function(
                    "closed_cleanup_owner",
                    0,
                    RetAbi::Zst,
                    Terminator::Return,
                )]
                .into(),
                ..Module::default()
            },
            false,
        );
        let control = std::sync::Arc::clone(owner.control());
        let thunk = thunks::get_or_create(owner.shared(), 0xe223, 0, &callback_sig());
        owner.wait_closed().unwrap();

        let foreign = engine(foreign_closed_traversal_module(thunk), jit);
        let exception = unwind::catch_raw(|| unsafe { run_export(&foreign, "probe", &[]) })
            .expect_err("the closed owner exception must cross the foreign Engine");
        let correctly_owned = exception.take_engine_closed(&control).is_ok();
        let cleanup_ran = FOREIGN_CLOSED_CLEANUP_RAN.load(Ordering::SeqCst);
        if !correctly_owned || cleanup_ran != 1 {
            failures.push(format!(
                "{mode}: owner={correctly_owned}, foreign-cleanup={cleanup_ran}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "EngineClosed skipped another Engine's guest cleanup: {failures:#?}"
    );
}

#[test]
fn engine_closed_obeys_a_guest_terminate_boundary() {
    const CHILD: &str = "MIRVM_ENGINE_CLOSED_TERMINATE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let owner = engine(
            Module {
                funcs: vec![function(
                    "closed_terminate_owner",
                    0,
                    RetAbi::Zst,
                    Terminator::Return,
                )]
                .into(),
                ..Module::default()
            },
            false,
        );
        let thunk = thunks::get_or_create(owner.shared(), 0xe224, 0, &callback_sig());
        owner.wait_closed().unwrap();
        let callback: unsafe extern "C-unwind" fn() =
            unsafe { std::mem::transmute(thunk as usize) };
        unwind::guard_terminate(|| unsafe { callback() });
        panic!("EngineClosed escaped a Terminate boundary");
    }

    let test_name =
        "vm::embed_tests::engine_lifecycle::close::engine_closed_obeys_a_guest_terminate_boundary";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("failed to start EngineClosed Terminate subprocess");
    assert!(
        !output.status.success(),
        "Terminate child returned normally"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unwind reached Terminate boundary"),
        "Terminate child failed for the wrong reason:\n{stderr}"
    );
}
