//! The close drain: a handler reraise and cross-engine synchronous raises.

use super::*;

static SIGNAL_CLOSE_CHAIN_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);

const SIGNAL_CLOSE_CHAIN_GUEST_ADDR: u64 = 0xe238;

fn physically_masked_reraising_signal_module() -> Module {
    let mut handler = function(
        "close_drain_handler_reraises_its_signal",
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
    handler.blocks[0]
        .stmts
        .push(marker_increment(&SIGNAL_OWNER_HANDLER_RAN));
    let mut module = Module {
        funcs: vec![handler].into(),
        ..Module::default()
    };
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_OWNER_GUEST_ADDR), 0));
    module
}

fn close_signal_source_module() -> Module {
    let handler = function(
        "close_drain_handler_raises_cross_engine_signal",
        1,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostRaise,
            args: vec![Operand::Imm {
                bits: crate::os::signal::SIGUSR2 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler].into(),
        ..Module::default()
    };
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_CLOSE_SOURCE_GUEST_ADDR), 0));
    module
}

fn close_signal_nine_delivery_module() -> Module {
    let count = word(0);
    let mut handler = function(
        "cross_engine_signal_reraises_until_ninth_delivery",
        1,
        RetAbi::Zst,
        Terminator::SwitchInt {
            discr: SwitchDiscr::Scalar(Operand::Slot(count)),
            targets: vec![(8, 2)],
            otherwise: 1,
        },
    );
    handler.blocks[0].stmts.push(Stmt::AtomicRmw {
        op: RmwOp::Add,
        addr: Operand::Imm {
            bits: SIGNAL_CLOSE_CHAIN_HANDLER_RAN.as_ptr() as u64,
            width: Width::W64,
        },
        val: Operand::Imm {
            bits: 1,
            width: Width::W64,
        },
        dst: ScalarPlace::Slot(count),
        order: MemOrd::SeqCst,
    });
    handler.blocks[1].term = Terminator::CallBuiltin {
        builtin: Builtin::HostRaise,
        args: vec![Operand::Imm {
            bits: crate::os::signal::SIGUSR2 as u64,
            width: Width::W32,
        }],
        ret: RetDest::Ignore,
        target: 2,
        unwind: UnwindAction::Continue,
        role: super::super::ir::BuiltinCallRole::Normal,
    };
    handler.blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Return,
    });
    let mut module = Module {
        funcs: vec![handler].into(),
        ..Module::default()
    };
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_CLOSE_CHAIN_GUEST_ADDR), 0));
    module
}

#[test]
fn close_drain_does_not_lose_a_handler_reraise_on_its_temporary_thread() {
    const CHILD: &str = "MIRVM_CLOSE_DRAIN_RERAISE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);

        for (_mode, jit) in modes() {
            SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
            SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
            let engine = engine(physically_masked_reraising_signal_module(), jit);
            super::super::signal::install_signal(
                engine.control(),
                crate::os::signal::SIGUSR1,
                SIGNAL_OWNER_GUEST_ADDR as usize,
                Some((0, SIGNAL_OWNER_GUEST_ADDR)),
            )
            .unwrap();

            assert_eq!(
                crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
                0
            );
            wait_for_owner_signal_pending(&engine);
            let mask = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
            let mask_guard = mask.block_for_handler(crate::os::signal::SIGUSR1).unwrap();
            engine.wait_closed().unwrap();
            assert_eq!(SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst), 1);
            assert_eq!(
                SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst),
                1,
                "close lost a signal reraised by the handler on its temporary finalizer thread"
            );
            assert_ne!(
                crate::os::signal::Sigaction::current_standard_mask_bits().unwrap()
                    & (1u64 << crate::os::signal::SIGUSR1),
                0,
                "Engine close changed the caller's preexisting pthread mask"
            );
            assert!(
                crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
                    .unwrap()
                    .same_disposition(&baseline)
            );
            drop(mask_guard);
            assert_eq!(SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst), 1);
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_close::drain::close_drain_does_not_lose_a_handler_reraise_on_its_temporary_thread";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start close-drain reraised-signal subprocess");
    let deadline = std::time::Instant::now() + CHILD_HANG_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("closing an Engine lost or stalled a signal reraised by its handler");
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
        "close-drain reraised-signal child failed:\n{stdout}{stderr}"
    );
}

#[test]
fn close_drain_exhausts_cross_engine_synchronous_raises_before_sealing() {
    const CHILD: &str = "MIRVM_CLOSE_DRAIN_NINTH_RAISE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore_usr1, baseline_usr1) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
        let (_restore_usr2, baseline_usr2) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR2);

        for (_mode, jit) in modes() {
            SIGNAL_CLOSE_CHAIN_HANDLER_RAN.store(0, Ordering::SeqCst);
            let source = engine(close_signal_source_module(), jit);
            let chain = engine(close_signal_nine_delivery_module(), jit);
            super::super::signal::install_signal(
                source.control(),
                crate::os::signal::SIGUSR1,
                SIGNAL_CLOSE_SOURCE_GUEST_ADDR as usize,
                Some((0, SIGNAL_CLOSE_SOURCE_GUEST_ADDR)),
            )
            .unwrap();
            super::super::signal::install_signal(
                chain.control(),
                crate::os::signal::SIGUSR2,
                SIGNAL_CLOSE_CHAIN_GUEST_ADDR as usize,
                Some((0, SIGNAL_CLOSE_CHAIN_GUEST_ADDR)),
            )
            .unwrap();

            assert_eq!(
                crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
                0
            );
            wait_for_owner_signal_pending(&source);
            let mask = raw_sigaction(crate::os::signal::SIG_DFL, &[crate::os::signal::SIGUSR2]);
            let mask_guard = mask.block_for_handler(crate::os::signal::SIGUSR1).unwrap();
            source.wait_closed().unwrap();
            assert_eq!(
                SIGNAL_CLOSE_CHAIN_HANDLER_RAN.load(Ordering::SeqCst),
                9,
                "close sealed while a ninth cross-Engine synchronous raise was still pending"
            );
            assert_eq!(source.state(), super::super::ctx::EngineState::Closed);
            assert_eq!(chain.state(), super::super::ctx::EngineState::Running);
            let physical = crate::os::signal::Sigaction::current_standard_mask_bits().unwrap();
            assert_ne!(physical & (1u64 << crate::os::signal::SIGUSR1), 0);
            assert_ne!(physical & (1u64 << crate::os::signal::SIGUSR2), 0);
            drop(mask_guard);
            chain.wait_closed().unwrap();
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
        return;
    }

    let test_name = "vm::embed_tests::signal_close::drain::close_drain_exhausts_cross_engine_synchronous_raises_before_sealing";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start ninth close-drain raise subprocess");
    let deadline = std::time::Instant::now() + CHILD_HANG_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("close did not finish its finite cross-Engine synchronous raise chain");
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
        "ninth close-drain raise child failed:\n{stdout}{stderr}"
    );
}
