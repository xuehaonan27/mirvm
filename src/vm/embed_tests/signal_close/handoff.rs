//! Handing finalization to another Engine or an unmasked thread.

use super::*;

static SIGNAL_MASKED_CLOSE_RETURNED: AtomicU64 = AtomicU64::new(0);

const SIGNAL_MASKING_CLOSE_GUEST_ADDR: u64 = 0xe235;

unsafe extern "C-unwind" fn process_kill_usr1() {
    assert_eq!(
        crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
        0
    );
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("signal owner Engine was not installed")
            .clone()
    });
    wait_for_owner_signal_pending(&engine);
}

unsafe extern "C-unwind" fn close_nested_engine_from_signal() {
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("masked close Engine was not installed")
            .clone()
    });
    engine.close();
    assert_eq!(
        SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst),
        0,
        "closing an Engine under another handler's mask finalized it before the handler returned"
    );
    SIGNAL_MASKED_CLOSE_RETURNED.store(1, Ordering::SeqCst);
}

fn masking_close_module(fault_after_close: bool) -> Module {
    let mut handler = function(
        "close_other_engine_while_its_signal_is_masked",
        1,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: close_nested_engine_from_signal as *const () as usize as u64,
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
    if fault_after_close {
        handler.blocks[1].term =
            Terminator::Trap("signal handler fault after requesting another Engine close".into());
    }
    let trigger = function(
        "raise_masking_close_signal",
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
        .push((LinkAddr(SIGNAL_MASKING_CLOSE_GUEST_ADDR), 0));
    module
}

fn process_kill_module() -> Module {
    let mut module = Module {
        funcs: vec![calls_native("process_kill_usr1", process_kill_usr1)].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module
}

#[test]
fn masked_cross_engine_close_hands_finalization_to_an_unmasked_thread() {
    const CHILD: &str = "MIRVM_MASKED_SIGNAL_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore_usr1, baseline_usr1) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
        let (_restore_usr2, baseline_usr2) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR2);

        for (_mode, jit) in modes() {
            for fault_after_close in [false, true] {
                SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
                SIGNAL_MASKED_CLOSE_RETURNED.store(0, Ordering::SeqCst);
                let pending_owner = engine(
                    signal_owner_module(SIGNAL_OWNER_GUEST_ADDR, &SIGNAL_OWNER_HANDLER_RAN),
                    jit,
                );
                let masking_owner = engine(masking_close_module(fault_after_close), jit);
                super::super::signal::install_signal(
                    pending_owner.control(),
                    crate::os::signal::SIGUSR2,
                    SIGNAL_OWNER_GUEST_ADDR as usize,
                    Some((0, SIGNAL_OWNER_GUEST_ADDR)),
                )
                .unwrap();
                let action = raw_sigaction(
                    SIGNAL_MASKING_CLOSE_GUEST_ADDR as usize,
                    &[crate::os::signal::SIGUSR2],
                );
                super::super::signal::install_sigaction(
                    masking_owner.control(),
                    crate::os::signal::SIGUSR1,
                    Some(action),
                    Some((0, SIGNAL_MASKING_CLOSE_GUEST_ADDR)),
                    0,
                )
                .unwrap();

                assert_eq!(
                    crate::os::signal::kill(
                        crate::os::process::getpid(),
                        crate::os::signal::SIGUSR2
                    ),
                    0
                );
                wait_for_owner_signal_pending(&pending_owner);
                let result = with_nested_engine(&pending_owner, || unsafe {
                    run_export(&masking_owner, "probe", &[])
                });
                pending_owner.wait_closed().unwrap();
                if fault_after_close {
                    assert!(matches!(
                        result,
                        Err(ref error) if error.kind == RunErrorKind::EngineFault
                    ));
                } else {
                    assert!(matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0));
                }
                assert_eq!(SIGNAL_MASKED_CLOSE_RETURNED.load(Ordering::SeqCst), 1);
                assert_eq!(SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst), 1);
                assert_eq!(
                    pending_owner.state(),
                    super::super::ctx::EngineState::Closed
                );
                masking_owner.wait_closed().unwrap();
                assert!(
                    crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
                        .unwrap()
                        .same_disposition(&baseline_usr1)
                );
                assert!(
                    crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR2)
                        .unwrap()
                        .same_disposition(&baseline_usr2)
                );
            }
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_close::handoff::masked_cross_engine_close_hands_finalization_to_an_unmasked_thread";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start masked signal close subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("closing an Engine under another handler's mask did not terminate");
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
        "masked signal close child failed:\n{stdout}{stderr}"
    );
}

#[test]
fn process_signal_waits_for_its_inactive_owner_while_another_engine_runs() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            signal_owner_module(SIGNAL_OWNER_GUEST_ADDR, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let active_foreign = engine(process_kill_module(), jit);
        super::super::signal::install_signal(
            owner.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let foreign_result = with_nested_engine(&owner, || unsafe {
            run_export(&active_foreign, "probe", &[])
        });
        let after_foreign = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let owner_result = unsafe { run_export(&owner, "probe", &[]) };
        let after_owner = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        active_foreign.wait_closed().unwrap();
        owner.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        if !matches!(foreign_result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || after_foreign != 0
            || !matches!(owner_result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || after_owner != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: foreign={foreign_result:?}, after-foreign={after_foreign}, owner={owner_result:?}, after-owner={after_owner}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "process signal ran in the wrong Engine activation: {failures:#?}"
    );
}
