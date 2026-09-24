//! Reinstating an old action: the guest-visible oldact and delivery through it.

use super::*;

static SIGNAL_OVERRIDE_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);

fn native_oldact_reinstall_module() -> Module {
    let old = word(0);
    let mut installer = function(
        "reinstall_native_signal_oldact",
        0,
        RetAbi::Scalar(old),
        Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: crate::os::signal::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::Imm {
                    bits: SIGNAL_OWNER_GUEST_ADDR,
                    width: Width::W64,
                },
            ],
            ret: RetDest::Scalar(ScalarPlace::Slot(old)),
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::super::ir::BuiltinCallRole::Normal,
        },
    );
    installer.blocks[1].term = Terminator::CallBuiltin {
        builtin: Builtin::HostSignal,
        args: vec![
            Operand::Imm {
                bits: crate::os::signal::SIGUSR1 as u64,
                width: Width::W32,
            },
            Operand::Slot(old),
        ],
        ret: RetDest::Ignore,
        target: 2,
        unwind: UnwindAction::Continue,
        role: super::super::ir::BuiltinCallRole::Normal,
    };
    installer.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Return,
    });

    let mut handler = function(
        "native_oldact_temporary_guest_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    handler.blocks[0]
        .stmts
        .push(marker_store(&SIGNAL_OWNER_HANDLER_RAN));
    let mut module = Module {
        funcs: vec![installer, handler].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_OWNER_GUEST_ADDR), 1));
    module
}

fn cross_engine_signal_reinstaller_module() -> Module {
    let old = word(0);
    let replaced = word(8);
    let mut handler = function(
        "cross_engine_middle_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    handler.blocks[0]
        .stmts
        .push(marker_store(&SIGNAL_OVERRIDE_HANDLER_RAN));

    let installer = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Scalar(old),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostSignal,
                    args: vec![
                        Operand::Imm {
                            bits: crate::os::signal::SIGUSR1 as u64,
                            width: Width::W32,
                        },
                        Operand::Imm {
                            bits: SIGNAL_OVERRIDE_GUEST_ADDR,
                            width: Width::W64,
                        },
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(old)),
                    target: 1,
                    unwind: UnwindAction::Continue,
                    role: super::super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostSignal,
                    args: vec![
                        Operand::Imm {
                            bits: crate::os::signal::SIGUSR1 as u64,
                            width: Width::W32,
                        },
                        Operand::Slot(old),
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(replaced)),
                    target: 2,
                    unwind: UnwindAction::Continue,
                    role: super::super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "reinstall_another_engine_signal_oldact".into(),
    };
    let safe_point = function(
        "cross_engine_signal_safe_point",
        0,
        RetAbi::Zst,
        Terminator::Return,
    );
    let mut module = Module {
        funcs: vec![handler, installer, safe_point].into(),
        ..Module::default()
    };
    module.exports.insert("install".into(), 1);
    module.exports.insert("probe".into(), 2);
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_OVERRIDE_GUEST_ADDR), 0));
    module
}

fn sigaction_reinstall_module() -> Module {
    let mut handler = function(
        "raw_oldact_reinstall_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    handler.blocks[0]
        .stmts
        .push(marker_store(&SIGNAL_OWNER_HANDLER_RAN));
    let sigaction = function(
        "reinstall_modified_raw_oldact",
        2,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostSigaction,
            args: vec![
                Operand::Imm {
                    bits: crate::os::signal::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::Slot(word(8)),
                Operand::Slot(word(16)),
            ],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, sigaction].into(),
        ..Module::default()
    };
    module.exports.insert("sigaction".into(), 1);
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_OWNER_GUEST_ADDR), 0));
    module
}

#[test]
fn guest_can_reinstall_a_native_signal_oldact_and_real_kill_uses_it() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(native_oldact_reinstall_module(), jit);
        let result = unsafe { run_export(&engine, "probe", &[]) };
        assert_eq!(
            crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
            0
        );
        wait_for_signal_marker(&SIGNAL_NATIVE_HANDLER_RAN);
        let native_ran = SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst);
        let guest_ran = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        if !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64)
            || native_ran != 1
            || guest_ran != 0
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: result={result:?}, native={native_ran}, guest={guest_ran}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "a native oldact could not be reinstalled through guest signal(): {failures:#?}"
    );
}

#[test]
fn another_engine_can_reinstall_guest_p1_oldact_without_stealing_ownership() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_OVERRIDE_HANDLER_RAN.store(0, Ordering::SeqCst);
        let link_addr = LinkAddr(SIGNAL_OWNER_GUEST_ADDR);
        let owner = engine(
            signal_p1_owner_module(link_addr, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let first_p1 = owner
            .shared()
            .instance
            .load_map
            .resolve(link_addr)
            .expect("signal P1 entry was not materialized");
        let installer = engine(cross_engine_signal_reinstaller_module(), jit);
        let owner_install = unsafe { run_export(&owner, "install", &[]) };

        let installed = unsafe { run_export(&installer, "install", &[]) };
        assert_eq!(
            crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
            0
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !super::super::signal::has_engine_pending(owner.control())
            && !super::super::signal::has_engine_pending(installer.control())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "cross-Engine signal was not published to either Engine inbox"
            );
            std::thread::yield_now();
        }
        let owner_received = super::super::signal::has_engine_pending(owner.control());
        let installer_was_idle = !super::super::signal::has_engine_pending(installer.control());
        let installer_safe = unsafe { run_export(&installer, "probe", &[]) };
        let owner_before_safe_point = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let owner_safe = unsafe { run_export(&owner, "probe", &[]) };
        let owner_after_safe_point = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);

        // A closes before B even though B installed the current A callback.
        // The surviving B disposition must become current, then B's close must
        // restore the native baseline.
        owner.wait_closed().unwrap();
        assert_eq!(
            crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
            0
        );
        wait_for_owner_signal_pending(&installer);
        let installer_after_owner_close = unsafe { run_export(&installer, "probe", &[]) };
        let override_after_safe_point = SIGNAL_OVERRIDE_HANDLER_RAN.load(Ordering::SeqCst);
        installer.wait_closed().unwrap();
        let target_first_restored =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        // Now reverse the close order. B owns both dispositions it installed,
        // including the one whose callback targets A. Closing B must reveal
        // A's original P1 registration without closing or stealing A.
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_OVERRIDE_HANDLER_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            signal_p1_owner_module(link_addr, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let p1 = owner
            .shared()
            .instance
            .load_map
            .resolve(link_addr)
            .expect("second signal P1 entry was not materialized");
        let installer = engine(cross_engine_signal_reinstaller_module(), jit);
        let second_owner_install = unsafe { run_export(&owner, "install", &[]) };
        let original_owner_kernel =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        let second_installed = unsafe { run_export(&installer, "install", &[]) };
        let installer_kernel =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        installer.wait_closed().unwrap();
        let after_installer_close =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        assert_eq!(
            crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
            0
        );
        wait_for_owner_signal_pending(&owner);
        let second_owner_safe = unsafe { run_export(&owner, "probe", &[]) };
        let second_owner_ran = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        owner.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        if !matches!(owner_install, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64)
            || !matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == first_p1)
            || !owner_received
            || !installer_was_idle
            || !matches!(installer_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || owner_before_safe_point != 0
            || !matches!(owner_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || owner_after_safe_point != 1
            || !matches!(installer_after_owner_close, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || override_after_safe_point != 1
            || !target_first_restored.same_disposition(&baseline)
            || !matches!(second_owner_install, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64)
            || !matches!(second_installed, Ok(RunOutcome::Returned(value)) if value.lo == p1)
            || installer_kernel.same_disposition(&original_owner_kernel)
            || !after_installer_close.same_disposition(&original_owner_kernel)
            || !matches!(second_owner_safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || second_owner_ran != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: owner-install={owner_install:?}, install={installed:?}, owner-received={owner_received}, installer-idle={installer_was_idle}, owner-before={owner_before_safe_point}, owner-after={owner_after_safe_point}, override-after={override_after_safe_point}, target-first-restored={}, second-owner-install={second_owner_install:?}, second-install={second_installed:?}, installer-stub-fresh={}, installer-close-restored-owner={}, second-owner={second_owner_ran}, restored={}",
                target_first_restored.same_disposition(&baseline),
                !installer_kernel.same_disposition(&original_owner_kernel),
                after_installer_close.same_disposition(&original_owner_kernel),
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "cross-Engine P1 oldact reinstall lost callback or installer ownership: {failures:#?}"
    );
}

#[test]
fn wrapped_sigaction_strips_fixed_stub_internals_from_a_modified_raw_oldact() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let engine = engine(sigaction_reinstall_module(), jit);
        super::super::signal::install_signal(
            engine.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let fixed_stub = baseline
            .replace(crate::os::signal::SIGUSR1)
            .expect("raw sigaction did not return MIRVM's fixed-stub oldact");
        let mut modified = fixed_stub;
        modified.or_flags(crate::os::signal::SA_NOCLDSTOP);
        modified.add_to_mask(crate::os::signal::SIGUSR2);
        assert_ne!(modified.flags() & crate::os::signal::SA_SIGINFO, 0);
        // The restorer is one of the internals this must strip, and the only one whose slot this
        // kernel's action may not have at all: where there is no slot, the modification carries no
        // restorer and there is nothing for this run to check it against.
        #[cfg(target_os = "linux")]
        assert!(modified.restorer().is_some());

        let mut old = crate::os::signal::Sigaction::empty(crate::os::signal::SIG_DFL, 0);
        let installed = unsafe {
            run_export(
                &engine,
                "sigaction",
                &[
                    std::ptr::from_ref(&modified) as u64,
                    std::ptr::from_mut(&mut old) as u64,
                ],
            )
        };
        let mut queried = crate::os::signal::Sigaction::empty(crate::os::signal::SIG_DFL, 0);
        let query = unsafe {
            run_export(
                &engine,
                "sigaction",
                &[0, std::ptr::from_mut(&mut queried) as u64],
            )
        };
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        let usr2_masked = queried.mask_contains(crate::os::signal::SIGUSR2) as i32;
        if !matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || !matches!(query, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || old.handler() != baseline.handler()
            || queried.handler() != SIGNAL_OWNER_GUEST_ADDR as usize
            || queried.flags() & crate::os::signal::SA_SIGINFO != 0
            || queried.flags() & crate::os::signal::SA_NOCLDSTOP == 0
            || queried.restorer().is_some()
            || usr2_masked != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: install={installed:?}, query={query:?}, old={:#x}, handler={:#x}, flags={:#x}, restorer={}, usr2-mask={usr2_masked}, restored={}",
                old.handler(),
                queried.handler(),
                queried.flags(),
                queried.restorer().is_some(),
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "wrapped sigaction leaked fixed-stub ABI details into the guest action: {failures:#?}"
    );
}
