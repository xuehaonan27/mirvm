//! Guest panics: how a panic is classified, caught and disposed.

use super::*;

static FOREIGN_CATCH_RAN: AtomicU64 = AtomicU64::new(0);

static THUNK_MAIN_CATCH_RESULT: AtomicU64 = AtomicU64::new(0);

unsafe extern "C-unwind" fn thunk_attempt_main_catch() {
    let ctx = super::super::ctx::current();
    let observed = if super::super::ctx::claim_main_panic_catch(
        ctx,
        super::super::ir::BuiltinCallRole::MainPanicCatcher,
    )
    .is_none()
    {
        1
    } else {
        2
    };
    THUNK_MAIN_CATCH_RESULT.store(observed, Ordering::SeqCst);
}

fn returns_101_function() -> FuncBody {
    let ret = word(0);
    let mut body = function(
        "main_returns_101",
        4,
        RetAbi::Scalar(ret),
        Terminator::Return,
    );
    body.blocks[0].stmts.push(Stmt::Assign {
        dst: ScalarPlace::Slot(ret),
        rv: Rvalue::Use(Operand::Imm {
            bits: 101,
            width: Width::W64,
        }),
    });
    body
}

fn main_module(body: FuncBody) -> Module {
    let mut module = Module {
        funcs: vec![body].into(),
        entry: Some(EntryPlan {
            lang_start: 0,
            main_addr: LinkAddr(0),
            argc: 0,
            argv_ptr: 0,
            sigpipe: 0,
        }),
        ..Module::default()
    };
    install_fake_guest_panic_cleanup(&mut module);
    module
}

fn caller_catches_owner_panic(thunk: u64) -> Module {
    let result = word(0);
    let try_addr = 0xb001;
    let catch_addr = 0xb002;
    let outer = function(
        "caller_catch_unwind",
        0,
        RetAbi::Scalar(result),
        Terminator::CallBuiltin {
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
            unwind: UnwindAction::Continue,
            role: crate::vm::ir::BuiltinCallRole::Normal,
        },
    );
    let try_body = function(
        "caller_try_invokes_owner_thunk",
        1,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: thunk,
                width: Width::W64,
            },
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            null_ok: false,
            native_sig: Some(callback_sig()),
        },
    );
    let mut catch_body = function(
        "caller_must_not_catch_owner_exception",
        2,
        RetAbi::Zst,
        Terminator::Return,
    );
    catch_body.blocks[0].stmts.push(Stmt::AtomicStore {
        addr: Operand::Imm {
            bits: FOREIGN_CATCH_RAN.as_ptr() as u64,
            width: Width::W64,
        },
        val: Operand::Imm {
            bits: 1,
            width: Width::W64,
        },
        order: MemOrd::SeqCst,
    });

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
fn guest_catch_does_not_consume_another_engines_panic() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        FOREIGN_CATCH_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            Module {
                funcs: vec![guest_panic_function("owner_guest_panic", 0)].into(),
                ..Module::default()
            },
            jit,
        );
        let thunk = thunks::get_or_create(owner.shared(), 0xa001, 0, &callback_sig());
        let caller = engine(caller_catches_owner_panic(thunk), jit);

        let exception = unwind::catch_raw(|| unsafe { run_export(&caller, "probe", &[]) })
            .expect_err("the owner Engine's guest panic must leave the caller Engine");
        let payload = match exception.take_mirvm(owner.shared()) {
            Ok(payload) => payload,
            Err(exception) => exception.resume_or_rethrow(),
        };
        let unwind::MirvmPayload::Guest(payload) = payload else {
            panic!("owner guest panic changed kind")
        };
        assert_eq!(payload.transfer(|_, inner| inner), 0x4747);
        if FOREIGN_CATCH_RAN.load(Ordering::SeqCst) != 0 {
            failures.push(mode);
        }
    }
    assert!(
        failures.is_empty(),
        "guest catch consumed a GuestPanic owned by another Engine in {failures:?}"
    );
}

#[test]
fn run_main_distinguishes_guest_panic_from_normal_exit_101() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let panicking = engine(main_module(guest_panic_function("main_panics", 4)), jit);
        let returning = engine(main_module(returns_101_function()), jit);
        let panic_result = run_main(&panicking);
        let return_result = run_main(&returning);

        if !matches!(panic_result, Ok(RunOutcome::GuestPanic))
            || !matches!(return_result, Ok(RunOutcome::Returned(101)))
        {
            failures.push((mode, panic_result, return_result));
        }
    }
    assert!(
        failures.is_empty(),
        "run_main collapsed guest panic and normal exit 101: {failures:?}"
    );
}

#[test]
fn run_result_has_structured_guest_panic_and_engine_fault_categories() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let guest = engine(export_module(guest_panic_function("export_panics", 0)), jit);
        let fault = engine(
            export_module(engine_fault_function("export_engine_fault", 0)),
            jit,
        );
        let guest_outcome = unsafe { run_export(&guest, "probe", &[]) }
            .expect("guest panic is a guest execution outcome");
        let fault_error = unsafe { run_export(&fault, "probe", &[]) }
            .expect_err("EngineFault must not be a successful export result");

        if guest_outcome != RunOutcome::GuestPanic || fault_error.kind != RunErrorKind::EngineFault
        {
            failures.push((mode, guest_outcome, fault_error));
        }
    }
    assert!(
        failures.is_empty(),
        "RunError has no structured guest-panic/engine-fault category: {failures:?}"
    );
}

#[test]
fn same_engine_thunk_reentry_cannot_claim_outer_main_catcher() {
    THUNK_MAIN_CATCH_RESULT.store(0, Ordering::SeqCst);
    let engine = engine(
        Module {
            funcs: vec![calls_native(
                "thunk_attempts_outer_main_catch",
                thunk_attempt_main_catch,
            )]
            .into(),
            ..Module::default()
        },
        false,
    );
    let code = thunks::get_or_create(engine.shared(), 0xe222, 0, &callback_sig());
    let callback: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(code as usize) };

    let activation = super::super::ctx::activate(engine.shared());
    let ctx = activation.ctx();
    let run = super::super::ctx::begin_main_run(ctx);
    super::super::ctx::call_main_panic_boundary(ctx, || {
        unsafe { callback() };
        assert_eq!(
            THUNK_MAIN_CATCH_RESULT.load(Ordering::SeqCst),
            1,
            "same-Engine thunk reentry claimed the outer main catcher"
        );
        let outer = super::super::ctx::claim_main_panic_catch(
            ctx,
            super::super::ir::BuiltinCallRole::MainPanicCatcher,
        );
        assert!(
            outer.is_some(),
            "outer activation could no longer claim its catcher"
        );
    });
    assert!(!run.finish());
    drop(activation);
    engine.wait_closed().unwrap();
}
