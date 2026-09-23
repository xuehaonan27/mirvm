//! libc error paths: sentinel returns, errno and signals MIRVM refuses.

use super::*;

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
                    role: super::super::ir::BuiltinCallRole::Normal,
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
                bits: crate::os::signal::SIGKILL as u64,
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
                    bits: crate::os::signal::realtime_min() as u64,
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
            role: super::super::ir::BuiltinCallRole::Normal,
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
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_OWNER_GUEST_ADDR), 4));
    module
}

#[test]
fn guest_signal_libc_errors_return_sentinels_and_errno() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        for export in ["invalid_signal", "invalid_sigaction", "sigkill"] {
            unsafe { *crate::os::process::errno_location() = 0 };
            let engine = engine(signal_libc_error_module(), jit);
            let result = unsafe { run_export(&engine, export, &[]) };
            engine.wait_closed().unwrap();
            if !matches!(
                result,
                Ok(RunOutcome::Returned(value))
                    if value.lo == u64::MAX && value.hi == crate::os::process::EINVAL as u64
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
