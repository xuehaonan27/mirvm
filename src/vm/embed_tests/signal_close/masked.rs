//! Close under a masked or deferred signal: what stays pending and what runs at unmask.

use super::*;

static LIFECYCLE_SIGNAL_NESTED_RAN: AtomicU64 = AtomicU64::new(0);

static LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN: AtomicU64 = AtomicU64::new(u64::MAX);

static SIGNAL_MASKED_OLD_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);

static SIGNAL_MASKED_NEW_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);

const LIFECYCLE_SIGNAL_GUEST_ADDR: u64 = 0xe221;

const SIGNAL_MASKED_OLD_GUEST_ADDR: u64 = 0xe233;

const SIGNAL_MASKED_NEW_GUEST_ADDR: u64 = 0xe234;

unsafe extern "C-unwind" fn lifecycle_close_and_record_signal() {
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("lifecycle reentry Engine was not installed")
            .clone()
    });
    engine.close();
    let _queue = engine.shared().jit.queue.lock().unwrap();
    assert_eq!(
        crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
        0
    );
    wait_for_owner_signal_pending(&engine);
    LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN.store(
        LIFECYCLE_SIGNAL_NESTED_RAN.load(Ordering::SeqCst),
        Ordering::SeqCst,
    );
}

fn physically_masked_close_child_finish(
    engine: Engine,
    mask_guard: crate::os::signal::ThreadSignalMaskGuard,
    baseline: &crate::os::signal::Sigaction,
    wrapped_raise: bool,
) {
    assert_ne!(
        crate::os::signal::Sigaction::current_standard_mask_bits().unwrap()
            & (1u64 << crate::os::signal::SIGUSR1),
        0
    );
    engine.wait_closed().unwrap();
    assert_eq!(
        SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst),
        u64::from(!wrapped_raise)
    );
    assert_eq!(engine.state(), super::super::ctx::EngineState::Closed);
    assert_ne!(
        crate::os::signal::Sigaction::current_standard_mask_bits().unwrap()
            & (1u64 << crate::os::signal::SIGUSR1),
        0,
        "Engine close changed the caller's preexisting pthread mask"
    );
    assert!(
        crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
            .unwrap()
            .same_disposition(baseline)
    );
    drop(mask_guard);
    if wrapped_raise {
        wait_for_signal_marker(&SIGNAL_NATIVE_HANDLER_RAN);
        assert_eq!(SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst), 1);
        assert_eq!(SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst), 0);
    } else {
        assert_eq!(SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst), 1);
    }
}

fn signal_nested_module() -> Module {
    let mut handler = function(
        "signal_handler_calls_nested_guest",
        1,
        RetAbi::Zst,
        Terminator::Call {
            callee: 2,
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::super::ir::CallRole::Normal,
        },
    );
    handler.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Return,
    });
    let nested = FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![marker_store(&LIFECYCLE_SIGNAL_NESTED_RAN)],
            term: Terminator::Return,
        }],
        name: "nested_guest_from_signal".into(),
    };
    let mut module = Module {
        funcs: vec![
            calls_native(
                "close_then_record_signal",
                lifecycle_close_and_record_signal,
            ),
            handler,
            nested,
        ]
        .into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module
        .fn_entry_links
        .push((LinkAddr(LIFECYCLE_SIGNAL_GUEST_ADDR), 1));
    module
}

fn signal_handler_waits_for_mask_deferred_engine_close_module() -> Module {
    let handler = function(
        "signal_handler_waits_for_another_engine_close",
        1,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: lifecycle_wait_from_callback as *const () as usize as u64,
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
    let trigger = function(
        "raise_signal_that_waits_for_another_engine_close",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: crate::os::signal::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, trigger].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 1);
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_CLOSE_SOURCE_GUEST_ADDR), 0));
    module
}

fn masked_raise_replacement_module() -> Module {
    let mut old_handler = function(
        "masked_raise_then_replace_handler",
        1,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: crate::os::signal::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::super::ir::BuiltinCallRole::Normal,
        },
    );
    old_handler.blocks[0]
        .stmts
        .push(marker_increment(&SIGNAL_MASKED_OLD_HANDLER_RAN));
    old_handler.blocks[1].term = Terminator::CallBuiltin {
        builtin: Builtin::HostRaise,
        args: vec![Operand::Imm {
            bits: crate::os::signal::SIGUSR1 as u64,
            width: Width::W32,
        }],
        ret: RetDest::Ignore,
        target: 2,
        unwind: UnwindAction::Continue,
        role: super::super::ir::BuiltinCallRole::Normal,
    };
    old_handler.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: crate::os::signal::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::Imm {
                    bits: SIGNAL_MASKED_NEW_GUEST_ADDR,
                    width: Width::W64,
                },
            ],
            ret: RetDest::Ignore,
            target: 3,
            unwind: UnwindAction::Continue,
            role: super::super::ir::BuiltinCallRole::Normal,
        },
    });
    old_handler.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Return,
    });

    let mut new_handler = function(
        "replacement_signal_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    new_handler.blocks[0]
        .stmts
        .push(marker_increment(&SIGNAL_MASKED_NEW_HANDLER_RAN));
    let trigger = function(
        "raise_initial_signal_handler",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: crate::os::signal::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![old_handler, new_handler, trigger].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 2);
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_MASKED_OLD_GUEST_ADDR), 0));
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_MASKED_NEW_GUEST_ADDR), 1));
    module
}

#[test]
fn preexisting_pthread_mask_blocks_wrapped_raise_and_deferred_inbox() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(physically_masked_signal_module(), jit);
        super::super::signal::install_signal(
            engine.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let mask = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
        let raise_guard = mask.block_for_handler(crate::os::signal::SIGUSR1).unwrap();
        let raised = unsafe { run_export(&engine, "raise", &[]) };
        let raised_while_masked = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        drop(raise_guard);
        let raised_safe = unsafe { run_export(&engine, "probe", &[]) };
        let raised_after_unmask = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);

        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        assert_eq!(
            crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
            0
        );
        wait_for_owner_signal_pending(&engine);
        let inbox_guard = mask.block_for_handler(crate::os::signal::SIGUSR1).unwrap();
        let inbox_masked_safe = unsafe { run_export(&engine, "probe", &[]) };
        let inbox_while_masked = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        drop(inbox_guard);
        let inbox_unmasked_safe = unsafe { run_export(&engine, "probe", &[]) };
        let inbox_after_unmask = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);

        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        if !matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || raised_while_masked != 0
            || !matches!(raised_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || raised_after_unmask != 1
            || !matches!(inbox_masked_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || inbox_while_masked != 0
            || !matches!(inbox_unmasked_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || inbox_after_unmask != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: raise={raised:?}, raise-masked={raised_while_masked}, raise-safe={raised_safe:?}, raise-after={raised_after_unmask}, inbox-masked-safe={inbox_masked_safe:?}, inbox-masked={inbox_while_masked}, inbox-safe={inbox_unmasked_safe:?}, inbox-after={inbox_after_unmask}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "the VM ignored the calling pthread's preexisting signal mask: {failures:#?}"
    );
}

#[test]
fn physically_masked_inbox_event_does_not_deadlock_engine_close() {
    const CHILD: &str = "MIRVM_PHYSICALLY_MASKED_SIGNAL_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);

        for (_mode, jit) in modes() {
            for wrapped_raise in [false, true] {
                SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
                SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
                let engine = engine(physically_masked_signal_module(), jit);
                super::super::signal::install_signal(
                    engine.control(),
                    crate::os::signal::SIGUSR1,
                    SIGNAL_OWNER_GUEST_ADDR as usize,
                    Some((0, SIGNAL_OWNER_GUEST_ADDR)),
                )
                .unwrap();

                let mask = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
                let mask_guard = if wrapped_raise {
                    let mask_guard = mask.block_for_handler(crate::os::signal::SIGUSR1).unwrap();
                    let raised = unsafe { run_export(&engine, "raise", &[]) };
                    assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
                    assert!(!super::super::signal::has_engine_pending(engine.control()));
                    mask_guard
                } else {
                    assert_eq!(
                        crate::os::signal::kill(
                            crate::os::process::getpid(),
                            crate::os::signal::SIGUSR1
                        ),
                        0
                    );
                    wait_for_owner_signal_pending(&engine);
                    mask.block_for_handler(crate::os::signal::SIGUSR1).unwrap()
                };

                physically_masked_close_child_finish(engine, mask_guard, &baseline, wrapped_raise);
            }
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_close::masked::physically_masked_inbox_event_does_not_deadlock_engine_close";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start physically masked signal close subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("closing an Engine with a physically masked inbox event did not terminate");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "physically masked signal close child failed:\n{stdout}{stderr}"
    );
}

#[test]
fn wait_closed_fails_fast_for_a_finalizer_deferred_by_the_current_signal_mask() {
    const CHILD: &str = "MIRVM_MASK_DEFERRED_WAIT_CLOSED_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);

        for (_mode, jit) in modes() {
            LIFECYCLE_WAIT_RESULT.store(0, Ordering::SeqCst);
            let target = engine(Module::default(), jit);
            let caller = engine(
                signal_handler_waits_for_mask_deferred_engine_close_module(),
                jit,
            );
            super::super::signal::install_signal(
                caller.control(),
                crate::os::signal::SIGUSR1,
                SIGNAL_CLOSE_SOURCE_GUEST_ADDR as usize,
                Some((0, SIGNAL_CLOSE_SOURCE_GUEST_ADDR)),
            )
            .unwrap();

            let result =
                with_nested_engine(&target, || unsafe { run_export(&caller, "probe", &[]) });
            assert!(matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert_eq!(
                LIFECYCLE_WAIT_RESULT.load(Ordering::SeqCst),
                1,
                "wait_closed did not report its current-thread deferred finalizer"
            );
            target.wait_closed().unwrap();
            assert_eq!(target.state(), super::super::ctx::EngineState::Closed);
            caller.wait_closed().unwrap();
            assert!(
                crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
                    .unwrap()
                    .same_disposition(&baseline)
            );
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_close::masked::wait_closed_fails_fast_for_a_finalizer_deferred_by_the_current_signal_mask";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start mask-deferred wait_closed subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("wait_closed blocked on a finalizer deferred by the current signal mask");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        status.success(),
        "mask-deferred wait_closed child failed:\n{stdout}{stderr}"
    );
}

#[test]
fn masked_raise_coalesces_and_uses_the_disposition_current_at_unmask() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_MASKED_OLD_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_MASKED_NEW_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(masked_raise_replacement_module(), jit);
        let old = super::super::signal::install_signal(
            engine.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_MASKED_OLD_GUEST_ADDR as usize,
            Some((0, SIGNAL_MASKED_OLD_GUEST_ADDR)),
        )
        .unwrap();
        let result = unsafe { run_export(&engine, "probe", &[]) };
        let old_ran = SIGNAL_MASKED_OLD_HANDLER_RAN.load(Ordering::SeqCst);
        let new_ran = SIGNAL_MASKED_NEW_HANDLER_RAN.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        if old != baseline.handler()
            || !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || old_ran != 1
            || new_ran != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: old={old:#x}, result={result:?}, old-ran={old_ran}, new-ran={new_ran}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "masked raise stayed bound to the old handler or did not coalesce: {failures:#?}"
    );
}

#[test]
fn closing_engine_defers_signal_until_a_safe_point_without_jit_lock_reentry() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        LIFECYCLE_SIGNAL_NESTED_RAN.store(0, Ordering::SeqCst);
        LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN.store(u64::MAX, Ordering::SeqCst);
        let engine = engine(signal_nested_module(), jit);
        let old = super::super::signal::install_signal(
            engine.control(),
            crate::os::signal::SIGUSR1,
            LIFECYCLE_SIGNAL_GUEST_ADDR as usize,
            Some((1, LIFECYCLE_SIGNAL_GUEST_ADDR)),
        )
        .unwrap();

        let result = with_nested_engine(&engine, || unsafe { run_export(&engine, "probe", &[]) });
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        if !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || old != baseline.handler()
            || LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN.load(Ordering::SeqCst) != 0
            || LIFECYCLE_SIGNAL_NESTED_RAN.load(Ordering::SeqCst) != 1
            || engine.state() != super::super::ctx::EngineState::Closed
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: result={result:?}, old={old:#x}, before_native_return={}, nested={}, state={:?}, restored={}",
                LIFECYCLE_SIGNAL_BEFORE_NATIVE_RETURN.load(Ordering::SeqCst),
                LIFECYCLE_SIGNAL_NESTED_RAN.load(Ordering::SeqCst),
                engine.state(),
                restored.same_disposition(&baseline),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "closing Engine did not defer signal delivery to an unlocked VM safe point: {failures:#?}"
    );
}
