//! Signal lifecycle tests: sigaction old-action visibility, external handler
//! adoption, libc error paths and native-image signal bridges.

use super::*;

const SIGNAL_IMAGE_NESTED_GUEST_ADDR: u64 = 0xe236;
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
                        bits: libc::SIGUSR1 as u64,
                        width: Width::W32,
                    }],
                    ret: RetDest::Ignore,
                    target: 3,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
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
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, trigger].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 1);
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 0);
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
                        bits: libc::SIGUSR1 as u64,
                        width: Width::W32,
                    }],
                    ret: RetDest::Ignore,
                    target: 3,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
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
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 0);
    module
}

fn libc_error_probe(name: &str, builtin: Builtin, args: Vec<Operand>) -> FuncBody {
    let result = word(0);
    let errno = word(8);
    FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Pair(result, errno),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin,
                    args,
                    ret: RetDest::Scalar(ScalarPlace::Slot(result)),
                    target: 1,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: read_current_errno as *const () as usize as u64,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Scalar(ScalarPlace::Slot(errno)),
                    target: 2,
                    unwind: UnwindAction::Continue,
                    null_ok: false,
                    native_sig: Some(ForeignSig {
                        args: Vec::new(),
                        ret: FfiKind::U64,
                        fixed: None,
                        thunk_args: Vec::new(),
                        unwind: true,
                    }),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: name.into(),
    }
}

fn signal_libc_error_module() -> Module {
    let invalid_signal = libc_error_probe(
        "signal_invalid_signum_returns_errno",
        Builtin::HostSignal,
        vec![
            Operand::Imm {
                bits: 0,
                width: Width::W32,
            },
            Operand::Imm {
                bits: crate::os::signal::SIG_DFL as u64,
                width: Width::W64,
            },
        ],
    );
    let invalid_sigaction = libc_error_probe(
        "sigaction_invalid_signum_query_returns_errno",
        Builtin::HostSigaction,
        vec![
            Operand::Imm {
                bits: 0,
                width: Width::W32,
            },
            Operand::Imm {
                bits: 0,
                width: Width::W64,
            },
            Operand::Imm {
                bits: 0,
                width: Width::W64,
            },
        ],
    );
    let uncatchable_signal = libc_error_probe(
        "signal_sigkill_returns_errno",
        Builtin::HostSignal,
        vec![
            Operand::Imm {
                bits: libc::SIGKILL as u64,
                width: Width::W32,
            },
            Operand::Imm {
                bits: SIGNAL_OWNER_GUEST_ADDR,
                width: Width::W64,
            },
        ],
    );
    let realtime_signal = function(
        "realtime_guest_signal_remains_unsupported",
        0,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostSignal,
            args: vec![
                Operand::Imm {
                    bits: libc::SIGRTMIN() as u64,
                    width: Width::W32,
                },
                Operand::Imm {
                    bits: SIGNAL_OWNER_GUEST_ADDR,
                    width: Width::W64,
                },
            ],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let handler = function(
        "signal_libc_error_guest_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    let mut module = Module {
        funcs: vec![
            invalid_signal,
            invalid_sigaction,
            uncatchable_signal,
            realtime_signal,
            handler,
        ]
        .into(),
        ..Module::default()
    };
    module.exports.insert("invalid_signal".into(), 0);
    module.exports.insert("invalid_sigaction".into(), 1);
    module.exports.insert("sigkill".into(), 2);
    module.exports.insert("realtime".into(), 3);
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 4);
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
                    bits: libc::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::AddrImm(link_addr),
            ],
            ret: RetDest::Scalar(ScalarPlace::Slot(old)),
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, installer].into(),
        ..Module::default()
    };
    module.exports.insert("install".into(), 1);
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
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
                bits: libc::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![trigger].into(),
        ..Module::default()
    };
    module.exports.insert("raise".into(), 0);
    module
}

fn native_image_signal_module(library: &Path, link_addr: LinkAddr) -> Module {
    let nested = function(
        "nested_guest_signal_handler",
        1,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "record_nested_guest".into(),
            sig: ForeignSig {
                args: vec![FfiKind::I32],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: Vec::new(),
                unwind: false,
            },
            args: vec![Operand::Slot(word(8))],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let install = function(
        "install_native_image_signal_handlers",
        0,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "install_image_handlers".into(),
            sig: ForeignSig {
                args: vec![FfiKind::Ptr],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: vec![(0, signal_callback_sig())],
                unwind: false,
            },
            args: vec![Operand::AddrImm(link_addr)],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let safe = function(
        "native_image_signal_safe_point",
        0,
        RetAbi::Zst,
        Terminator::Return,
    );
    let mut module = Module {
        funcs: vec![nested, install, safe].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.exports.insert("install".into(), 1);
    module.exports.insert("safe".into(), 2);
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: signal_callback_sig(),
    });
    module
}

fn native_image_signal_fault_module(library: &Path, link_addr: LinkAddr) -> Module {
    let nested_fault = engine_fault_function("nested_guest_signal_engine_fault", 1);
    let install = function(
        "install_native_image_fault_signal_handlers",
        0,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "install_image_fault_handlers".into(),
            sig: ForeignSig {
                args: vec![FfiKind::Ptr],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: vec![(0, signal_callback_sig())],
                unwind: false,
            },
            args: vec![Operand::AddrImm(link_addr)],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let safe = function(
        "native_image_fault_signal_safe_point",
        0,
        RetAbi::Zst,
        Terminator::Return,
    );
    let mut module = Module {
        funcs: vec![nested_fault, install, safe].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.exports.insert("install".into(), 1);
    module.exports.insert("safe".into(), 2);
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: signal_callback_sig(),
    });
    module
}

fn native_image_signal_bridge_error_module(library: &Path, link_addr: LinkAddr) -> Module {
    fn probe(
        name: &str,
        symbol: &str,
        link_addr: Option<LinkAddr>,
        returns_value: bool,
    ) -> FuncBody {
        let result = word(0);
        let args = link_addr
            .map(|address| vec![Operand::AddrImm(address)])
            .unwrap_or_default();
        function(
            name,
            0,
            if returns_value {
                RetAbi::Scalar(result)
            } else {
                RetAbi::Zst
            },
            Terminator::CallForeign {
                sym: symbol.into(),
                sig: ForeignSig {
                    args: if link_addr.is_some() {
                        vec![FfiKind::Ptr]
                    } else {
                        Vec::new()
                    },
                    ret: if returns_value {
                        FfiKind::U64
                    } else {
                        FfiKind::Void
                    },
                    fixed: None,
                    thunk_args: link_addr
                        .map(|_| vec![(0, signal_callback_sig())])
                        .unwrap_or_default(),
                    unwind: true,
                },
                args,
                ret: if returns_value {
                    RetDest::Scalar(ScalarPlace::Slot(result))
                } else {
                    RetDest::Ignore
                },
                target: 1,
                unwind: UnwindAction::Continue,
            },
        )
    }

    let handler = function(
        "native_signal_bridge_error_guest_handler",
        1,
        RetAbi::Zst,
        Terminator::Return,
    );
    let funcs = vec![
        handler,
        probe(
            "native_signal_invalid_signum",
            "image_signal_invalid_signum",
            None,
            true,
        ),
        probe(
            "native_sigaction_invalid_signum",
            "image_sigaction_invalid_signum",
            None,
            true,
        ),
        probe(
            "native_signal_sigkill",
            "image_signal_sigkill",
            Some(link_addr),
            true,
        ),
        probe(
            "native_sigaction_sigkill",
            "image_sigaction_sigkill",
            Some(link_addr),
            true,
        ),
        probe(
            "native_signal_realtime_contract",
            "image_signal_realtime",
            Some(link_addr),
            false,
        ),
        probe(
            "native_signal_sync_fault_contract",
            "image_signal_sync_fault",
            Some(link_addr),
            false,
        ),
        probe(
            "native_sigaction_siginfo_contract",
            "image_sigaction_siginfo",
            Some(link_addr),
            false,
        ),
    ];
    let mut module = Module {
        funcs: funcs.into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    for (name, func) in [
        ("invalid_signal", 1),
        ("invalid_sigaction", 2),
        ("sigkill", 3),
        ("sigaction_sigkill", 4),
        ("realtime", 5),
        ("sync_fault", 6),
        ("siginfo", 7),
    ] {
        module.exports.insert(name.into(), func);
    }
    module.fn_addrs.insert(link_addr.0, 0);
    module.link_fn_addrs.insert(link_addr, 0);
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: signal_callback_sig(),
    });
    module
}

fn native_image_closed_signal_handler_module(library: &Path, handler: u64) -> Module {
    let probe = function(
        "native_signal_closed_mirvm_handler_contract",
        0,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "image_signal_closed_handler".into(),
            sig: ForeignSig {
                args: vec![FfiKind::Ptr],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: Vec::new(),
                unwind: true,
            },
            args: vec![Operand::Imm {
                bits: handler,
                width: Width::W64,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let mut module = Module {
        funcs: vec![probe].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.exports.insert("closed".into(), 0);
    module
}

#[test]
fn signal_oldact_stays_guest_visible_and_non_lifo_close_restores_native_action() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let owner = engine(
            signal_owner_module(SIGNAL_OWNER_GUEST_ADDR, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let override_engine = engine(
            signal_owner_module(SIGNAL_OVERRIDE_GUEST_ADDR, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );

        let owner_old = super::signal::install_signal(
            owner.control(),
            libc::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();
        let mut owner_query: libc::sigaction = unsafe { std::mem::zeroed() };
        super::signal::install_sigaction(
            owner.control(),
            libc::SIGUSR1,
            None,
            None,
            std::ptr::from_mut(&mut owner_query) as u64,
        )
        .unwrap();

        let override_old = super::signal::install_signal(
            override_engine.control(),
            libc::SIGUSR1,
            SIGNAL_OVERRIDE_GUEST_ADDR as usize,
            Some((0, SIGNAL_OVERRIDE_GUEST_ADDR)),
        )
        .unwrap();
        owner.wait_closed().unwrap();

        let mut override_query: libc::sigaction = unsafe { std::mem::zeroed() };
        super::signal::install_sigaction(
            override_engine.control(),
            libc::SIGUSR1,
            None,
            None,
            std::ptr::from_mut(&mut override_query) as u64,
        )
        .unwrap();
        override_engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if owner_old != baseline.handler()
            || owner_query.sa_sigaction != SIGNAL_OWNER_GUEST_ADDR as usize
            || override_old != SIGNAL_OWNER_GUEST_ADDR as usize
            || override_query.sa_sigaction != SIGNAL_OVERRIDE_GUEST_ADDR as usize
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: owner-old={owner_old:#x}, owner-query={:#x}, override-old={override_old:#x}, override-query={:#x}, restored={}",
                owner_query.sa_sigaction,
                override_query.sa_sigaction,
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "guest oldact translation or non-LIFO signal restoration failed: {failures:#?}"
    );
}

#[test]
fn guest_signal_accepts_a_first_seen_external_native_handler() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_FIRST_EXTERNAL_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(first_external_native_signal_module(), jit);
        let installed = unsafe { run_export(&engine, "install", &[]) };
        let current = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_signal_marker(&SIGNAL_FIRST_EXTERNAL_HANDLER_RAN);
        let external_ran = SIGNAL_FIRST_EXTERNAL_HANDLER_RAN.load(Ordering::SeqCst);
        let baseline_ran = SIGNAL_NATIVE_HANDLER_RAN.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

        if !matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == baseline.handler() as u64)
            || current.handler() != first_external_native_signal as *const () as usize
            || external_ran != 1
            || baseline_ran != 0
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: install={installed:?}, current={:#x}, external={external_ran}, baseline={baseline_ran}, restored={}",
                current.handler(),
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "guest signal rejected or failed to restore a first-seen external native handler: {failures:#?}"
    );
}

#[test]
fn guest_signal_libc_errors_return_sentinels_and_errno() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        for export in ["invalid_signal", "invalid_sigaction", "sigkill"] {
            unsafe { *libc::__errno_location() = 0 };
            let engine = engine(signal_libc_error_module(), jit);
            let result = unsafe { run_export(&engine, export, &[]) };
            engine.wait_closed().unwrap();
            if !matches!(
                result,
                Ok(RunOutcome::Returned(value))
                    if value.lo == u64::MAX && value.hi == libc::EINVAL as u64
            ) {
                failures.push(format!("{mode}/{export}: {result:?}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "libc signal errors did not return SIG_ERR/-1 with EINVAL: {failures:#?}"
    );
}

#[test]
fn unsupported_realtime_guest_signal_remains_an_engine_fault() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let engine = engine(signal_libc_error_module(), jit);
        let result = unsafe { run_export(&engine, "realtime", &[]) };
        engine.wait_closed().unwrap();
        if !matches!(result, Err(ref error) if error.kind == RunErrorKind::EngineFault) {
            failures.push(format!("{mode}: {result:?}"));
        }
    }

    assert!(
        failures.is_empty(),
        "unsupported realtime guest signal stopped failing loudly: {failures:#?}"
    );
}

#[test]
fn self_produced_native_signal_bridge_preserves_libc_sentinels_and_errno() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_directory, library) = native_signal_handler_library();
    let expected = ((libc::EINVAL as u64) << 32) | 1;
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let engine = engine(
            native_image_signal_bridge_error_module(
                &library,
                LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR),
            ),
            jit,
        );
        for export in [
            "invalid_signal",
            "invalid_sigaction",
            "sigkill",
            "sigaction_sigkill",
        ] {
            let result = unsafe { run_export(&engine, export, &[]) };
            if !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == expected) {
                failures.push(format!("{mode}/{export}: {result:?}"));
            }
        }
        engine.wait_closed().unwrap();
    }

    assert!(
        failures.is_empty(),
        "self-produced native signal bridge lost libc sentinel/errno results: {failures:#?}"
    );
}

#[test]
fn self_produced_native_signal_bridge_raises_contract_errors_at_its_engine_boundary() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline_usr1) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let baseline_segv = crate::os::signal::Sigaction::query(libc::SIGSEGV).unwrap();
    let baseline_realtime = crate::os::signal::Sigaction::query(libc::SIGRTMIN()).unwrap();
    let (_directory, library) = native_signal_handler_library();
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        for (export, message) in [
            ("realtime", "outside the supported traditional signal range"),
            ("sync_fault", "synchronous fault signal"),
            ("siginfo", "uses unsupported SA_SIGINFO"),
        ] {
            let engine = engine(
                native_image_signal_bridge_error_module(
                    &library,
                    LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR),
                ),
                jit,
            );
            let result = unsafe { run_export(&engine, export, &[]) };
            engine.wait_closed().unwrap();
            if !matches!(result, Err(ref error) if error.kind == RunErrorKind::EngineFault && error.message.contains(message))
            {
                failures.push(format!("{mode}/{export}: {result:?}"));
            }
        }

        let link_addr = LinkAddr(0x6b00_0000_0600);
        let owner = engine(
            signal_p1_owner_module(link_addr, &SIGNAL_OWNER_HANDLER_RAN),
            jit,
        );
        let closed_handler = owner.shared().module.resolve_link_addr(link_addr);
        owner.wait_closed().unwrap();
        let engine = engine(
            native_image_closed_signal_handler_module(&library, closed_handler),
            jit,
        );
        let result = unsafe { run_export(&engine, "closed", &[]) };
        engine.wait_closed().unwrap();
        if !matches!(
            result,
            Err(ref error)
                if error.kind == RunErrorKind::EngineFault
                    && error.message.contains("closed owner")
        ) {
            failures.push(format!("{mode}/closed: {result:?}"));
        }
    }

    let restored_usr1 = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
    let restored_segv = crate::os::signal::Sigaction::query(libc::SIGSEGV).unwrap();
    let restored_realtime = crate::os::signal::Sigaction::query(libc::SIGRTMIN()).unwrap();
    assert!(restored_usr1.same_disposition(&baseline_usr1));
    assert!(restored_segv.same_disposition(&baseline_segv));
    assert!(restored_realtime.same_disposition(&baseline_realtime));
    assert!(
        failures.is_empty(),
        "self-produced native signal bridge flattened contract errors: {failures:#?}"
    );
}

#[test]
fn self_produced_native_signal_handler_runs_at_safe_point_and_can_raise_guest_handler() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore_usr1, baseline_usr1) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let (_restore_usr2, baseline_usr2) = SavedSignalDisposition::replace_with_native(libc::SIGUSR2);
    let (_directory, library) = native_signal_handler_library();
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let preexisting_raw = raw_sigaction(crate::os::signal::SIG_DFL, &[libc::SIGHUP]);
        let preexisting = unsafe {
            crate::os::signal::Sigaction::copy_from(std::ptr::from_ref(&preexisting_raw) as u64)
                .unwrap()
        };
        let preexisting_guard = preexisting.block_for_handler(libc::SIGWINCH).unwrap();
        let engine = engine(
            native_image_signal_module(&library, LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR)),
            jit,
        );
        let install = unsafe { run_export(&engine, "install", &[]) };
        let image = &engine.shared().module.native_images[0];
        let trace_address = crate::os::dll::sym(image.handle(), c"read_image_signal_trace");
        assert_ne!(trace_address, 0, "native trace reader was not exported");
        let read_trace: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(trace_address) };

        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_owner_signal_pending(&engine);
        let before_safe_point = unsafe { read_trace() };
        let safe = unsafe { run_export(&engine, "safe", &[]) };
        let after_safe_point = unsafe { read_trace() };
        let preserved_preexisting_mask = crate::os::signal::Sigaction::current_standard_mask_bits()
            .unwrap()
            & (1u64 << libc::SIGHUP)
            != 0;
        drop(preexisting_guard);
        engine.wait_closed().unwrap();
        let restored_usr1 = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        let restored_usr2 = crate::os::signal::Sigaction::query(libc::SIGUSR2).unwrap();

        if !matches!(install, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || before_safe_point != 0
            || !matches!(safe, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || after_safe_point != 123
            || !preserved_preexisting_mask
            || !restored_usr1.same_disposition(&baseline_usr1)
            || !restored_usr2.same_disposition(&baseline_usr2)
        {
            failures.push(format!(
                "{mode}: install={install:?}, before={before_safe_point}, safe={safe:?}, after={after_safe_point}, preexisting-mask={preserved_preexisting_mask}, usr1-restored={}, usr2-restored={}",
                restored_usr1.same_disposition(&baseline_usr1),
                restored_usr2.same_disposition(&baseline_usr2),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "self-produced native signal handler did not stay out of the kernel frame: {failures:#?}"
    );
}

#[test]
fn image_native_wrapped_raise_resumes_guest_engine_fault_to_its_owner_boundary() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore_usr1, baseline_usr1) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
    let (_restore_usr2, baseline_usr2) = SavedSignalDisposition::replace_with_native(libc::SIGUSR2);
    let (_directory, library) = native_signal_handler_library();
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let engine = engine(
            native_image_signal_fault_module(&library, LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR)),
            jit,
        );
        let install = unsafe { run_export(&engine, "install", &[]) };
        let image = &engine.shared().module.native_images[0];
        let trace_address = crate::os::dll::sym(image.handle(), c"read_image_signal_trace");
        assert_ne!(trace_address, 0, "native trace reader was not exported");
        let read_trace: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(trace_address) };

        assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGUSR1) }, 0);
        wait_for_owner_signal_pending(&engine);
        let before_safe_point = unsafe { read_trace() };
        let safe = unsafe { run_export(&engine, "safe", &[]) };
        let after_fault = unsafe { read_trace() };
        engine.wait_closed().unwrap();
        let restored_usr1 = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();
        let restored_usr2 = crate::os::signal::Sigaction::query(libc::SIGUSR2).unwrap();

        if !matches!(install, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || before_safe_point != 0
            || !matches!(
                safe,
                Err(ref error)
                    if error.kind == RunErrorKind::EngineFault
                        && error.message.contains("embedding contract engine fault")
            )
            || after_fault != 4
            || !restored_usr1.same_disposition(&baseline_usr1)
            || !restored_usr2.same_disposition(&baseline_usr2)
        {
            failures.push(format!(
                "{mode}: install={install:?}, before={before_safe_point}, safe={safe:?}, after={after_fault}, usr1-restored={}, usr2-restored={}",
                restored_usr1.same_disposition(&baseline_usr1),
                restored_usr2.same_disposition(&baseline_usr2),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "wrapped native raise did not return the guest EngineFault to its owner: {failures:#?}"
    );
}

#[test]
fn cross_engine_synchronous_signal_fault_returns_to_the_raising_boundary() {
    const CHILD: &str = "MIRVM_CROSS_ENGINE_SIGNAL_FAULT_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
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
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

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

    let test_name = "vm::engine::embed_tests::signal_lifecycle::cross_engine_synchronous_signal_fault_returns_to_the_raising_boundary";
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
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let jit = mode == "jit";
        let owner = engine(mask_restored_faulting_signal_owner_module(), jit);
        let raiser = engine(cross_engine_signal_raiser_module(), jit);
        super::signal::install_signal(
            owner.control(),
            libc::SIGUSR1,
            SIGNAL_OWNER_GUEST_ADDR as usize,
            Some((0, SIGNAL_OWNER_GUEST_ADDR)),
        )
        .unwrap();

        let raised = unsafe { run_export(&raiser, "raise", &[]) };
        let delivered = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        let raiser_closed = raiser.wait_closed();
        let owner_closed = owner.wait_closed();
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

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

    let test_name = "vm::engine::embed_tests::signal_lifecycle::cross_engine_mask_restored_signal_fault_returns_to_the_raising_boundary";
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
        let (_restore, baseline) = SavedSignalDisposition::replace_with_native(libc::SIGUSR1);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_NATIVE_HANDLER_RAN.store(0, Ordering::SeqCst);
        let jit = mode == "jit";
        let engine = engine(faulting_masked_reraise_module(), jit);
        super::signal::install_signal(
            engine.control(),
            libc::SIGUSR1,
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
        let restored = crate::os::signal::Sigaction::query(libc::SIGUSR1).unwrap();

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

    let test_name = "vm::engine::embed_tests::signal_lifecycle::faulting_handler_same_signal_raise_survives_cross_thread_close";
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
