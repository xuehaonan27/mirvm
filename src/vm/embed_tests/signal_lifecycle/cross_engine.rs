//! A signal fault that crosses an Engine boundary, and its close behavior.

use super::*;

const SIGNAL_FAULTING_P1_GUEST_ADDR: u64 = 0xe239;

fn faulting_masked_reraise_module() -> Module {
    let count = word(0);
    let handler = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(word(8))],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: vec![Stmt::AtomicRmw {
                    op: RmwOp::Add,
                    addr: Operand::Imm {
                        bits: SIGNAL_OWNER_HANDLER_RAN.as_ptr() as u64,
                        width: Width::W64,
                    },
                    val: Operand::Imm {
                        bits: 1,
                        width: Width::W64,
                    },
                    dst: ScalarPlace::Slot(count),
                    order: MemOrd::SeqCst,
                }],
                term: Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(Operand::Slot(count)),
                    targets: vec![(0, 1)],
                    otherwise: 4,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: unblock_usr1_inside_signal_handler as *const () as usize as u64,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 2,
                    unwind: UnwindAction::Continue,
                    null_ok: false,
                    native_sig: Some(callback_sig()),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostRaise,
                    args: vec![Operand::Imm {
                        bits: crate::os::signal::SIGUSR1 as u64,
                        width: Width::W32,
                    }],
                    ret: RetDest::Ignore,
                    target: 3,
                    unwind: UnwindAction::Continue,
                    role: super::super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Trap(
                    "signal handler trapped after a successful same-number raise".into(),
                ),
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "fault_after_masked_same_signal_raise".into(),
    };
    let trigger = function(
        "raise_faulting_masked_signal_handler",
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
        .push((LinkAddr(SIGNAL_OWNER_GUEST_ADDR), 0));
    module
}

fn mask_restored_faulting_signal_owner_module() -> Module {
    let count = word(0);
    let handler = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(word(8))],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: vec![Stmt::AtomicRmw {
                    op: RmwOp::Add,
                    addr: Operand::Imm {
                        bits: SIGNAL_OWNER_HANDLER_RAN.as_ptr() as u64,
                        width: Width::W64,
                    },
                    val: Operand::Imm {
                        bits: 1,
                        width: Width::W64,
                    },
                    dst: ScalarPlace::Slot(count),
                    order: MemOrd::SeqCst,
                }],
                term: Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(Operand::Slot(count)),
                    targets: vec![(0, 1)],
                    otherwise: 2,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostRaise,
                    args: vec![Operand::Imm {
                        bits: crate::os::signal::SIGUSR1 as u64,
                        width: Width::W32,
                    }],
                    ret: RetDest::Ignore,
                    target: 3,
                    unwind: UnwindAction::Continue,
                    role: super::super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Trap(
                    "mask-restored same-signal callback faulted on its second delivery".into(),
                ),
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "fault_when_mask_restoration_delivers_same_signal".into(),
    };
    let mut module = Module {
        funcs: vec![handler].into(),
        ..Module::default()
    };
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_OWNER_GUEST_ADDR), 0));
    module
}

fn faulting_signal_p1_owner_module(link_addr: LinkAddr) -> Module {
    let handler = function(
        "cross_engine_faulting_signal_handler",
        1,
        RetAbi::Zst,
        Terminator::Trap("cross-Engine synchronous signal handler fault".into()),
    );
    let old = word(0);
    let installer = function(
        "install_cross_engine_faulting_p1_handler",
        0,
        RetAbi::Scalar(old),
        Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: crate::os::signal::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::AddrImm(link_addr),
            ],
            ret: RetDest::Scalar(ScalarPlace::Slot(old)),
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, installer].into(),
        ..Module::default()
    };
    module.exports.insert("install".into(), 1);
    module.fn_entry_links.push((link_addr, 0));
    module
        .entry_stub_sites
        .push(super::super::ir::EntryStubSite {
            link_addr,
            func: 0,
            sig: signal_callback_sig(),
        });
    module
}

fn cross_engine_signal_raiser_module() -> Module {
    let trigger = function(
        "raise_another_engines_faulting_handler",
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
        funcs: vec![trigger].into(),
        ..Module::default()
    };
    module.exports.insert("raise".into(), 0);
    module
}

#[test]
fn cross_engine_synchronous_signal_fault_returns_to_the_raising_boundary() {
    const CHILD: &str = "MIRVM_CROSS_ENGINE_SIGNAL_FAULT_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
        let jit = mode == "jit";
        let owner = engine(
            faulting_signal_p1_owner_module(LinkAddr(SIGNAL_FAULTING_P1_GUEST_ADDR)),
            jit,
        );
        let raiser = engine(cross_engine_signal_raiser_module(), jit);

        let installed = unsafe { run_export(&owner, "install", &[]) };
        let raised = unsafe { run_export(&raiser, "raise", &[]) };
        let raiser_closed = raiser.wait_closed();
        let owner_closed = owner.wait_closed();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        assert!(
            matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64),
            "faulting P1 handler did not install through HostSignal: {installed:?}"
        );
        assert!(
            matches!(
                raised,
                Err(ref error)
                    if error.kind == RunErrorKind::EngineFault
                        && error.exit_code == 70
                        && error
                            .message
                            .contains("cross-Engine synchronous signal handler fault")
            ),
            "the raising Engine did not receive a structured handler fault: {raised:?}"
        );
        assert!(
            raiser_closed.is_ok(),
            "raising Engine could not close: {raiser_closed:?}"
        );
        assert!(
            owner_closed.is_ok(),
            "handler Engine could not close: {owner_closed:?}"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::embed_tests::signal_lifecycle::cross_engine::cross_engine_synchronous_signal_fault_returns_to_the_raising_boundary";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, mode)
            .output()
            .expect("failed to start cross-Engine signal-fault subprocess");
        if !output.status.success() {
            failures.push(format!(
                "{mode}: status={}\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "cross-Engine synchronous signal fault escaped both owners: {failures:#?}"
    );
}

#[test]
fn cross_engine_mask_restored_signal_fault_returns_to_the_raising_boundary() {
    const CHILD: &str = "MIRVM_CROSS_ENGINE_MASK_RESTORED_FAULT_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let jit = mode == "jit";
        let owner = engine(mask_restored_faulting_signal_owner_module(), jit);
        let raiser = engine(cross_engine_signal_raiser_module(), jit);
        super::super::signal::install_signal(
            owner.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let raised = unsafe { run_export(&raiser, "raise", &[]) };
        let delivered = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let raiser_closed = raiser.wait_closed();
        let owner_closed = owner.wait_closed();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        assert!(
            matches!(
                raised,
                Err(ref error)
                    if error.kind == RunErrorKind::EngineFault
                        && error
                            .message
                            .contains("mask-restored same-signal callback faulted")
            ),
            "A did not receive B's mask-restoration fault: {raised:?}"
        );
        assert_eq!(
            delivered, 2,
            "B's successful masked reraise was not delivered exactly once"
        );
        assert!(
            raiser_closed.is_ok(),
            "raising Engine could not close: {raiser_closed:?}"
        );
        assert!(
            owner_closed.is_ok(),
            "callback Engine could not close: {owner_closed:?}"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::embed_tests::signal_lifecycle::cross_engine::cross_engine_mask_restored_signal_fault_returns_to_the_raising_boundary";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, mode)
            .output()
            .expect("failed to start mask-restored signal-fault subprocess");
        if !output.status.success() {
            failures.push(format!(
                "{mode}: status={}\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "mask-restored cross-Engine fault escaped A's boundary: {failures:#?}"
    );
}

#[test]
fn faulting_handler_same_signal_raise_survives_cross_thread_close() {
    const CHILD: &str = "MIRVM_FAULTING_MASKED_RERAISE_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
        let jit = mode == "jit";
        let engine = engine(faulting_masked_reraise_module(), jit);
        super::super::signal::install_signal(
            engine.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let result = unsafe { run_export(&engine, "probe", &[]) };
        let before_close = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let closer = engine.clone();
        let closed = std::thread::spawn(move || closer.wait_closed())
            .join()
            .expect("cross-thread close panicked");
        let after_close = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let native_after_close = SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst);
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        assert!(
            matches!(result, Err(ref error) if error.kind == RunErrorKind::EngineFault),
            "faulting signal handler returned the wrong result: {result:?}"
        );
        assert_eq!(
            before_close, 2,
            "the EngineFault boundary did not drain the accepted same-number raise"
        );
        assert!(
            closed.is_ok(),
            "cross-thread Engine close failed: {closed:?}"
        );
        assert_eq!(
            after_close, 2,
            "Engine close changed the already-drained same-number raise"
        );
        assert_eq!(
            native_after_close, 0,
            "the accepted guest raise crossed into the restored native baseline"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::embed_tests::signal_lifecycle::cross_engine::faulting_handler_same_signal_raise_survives_cross_thread_close";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, mode)
            .output()
            .expect("failed to start faulting masked-reraise subprocess");
        if !output.status.success() {
            failures.push(format!(
                "{mode}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "same-number raise vanished after its handler faulted: {failures:#?}"
    );
}
