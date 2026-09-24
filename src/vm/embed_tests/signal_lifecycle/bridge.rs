//! Native-image signal bridges: self-produced raises routed through a native image.

use super::*;

const SIGNAL_IMAGE_NESTED_GUEST_ADDR: u64 = 0xe236;

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
fn self_produced_native_signal_bridge_preserves_libc_sentinels_and_errno() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_directory, library) = native_signal_handler_library();
    let expected = ((crate::os::process::EINVAL as u64) << 32) | 1;
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
    let (_restore, baseline_usr1) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let baseline_segv = crate::os::signal::Sigaction::query(crate::os::signal::SIGSEGV).unwrap();
    // A platform that numbers no realtime signals refuses a `sigaction` for the number
    // [`realtime_min`] names with `EINVAL`, so there is no disposition to save and no guest request
    // for one to answer with an Engine fault; that platform's check that a *valid* number is
    // refused for a semantic reason is the synchronous-fault case below, which both platforms run.
    #[cfg(target_os = "linux")]
    let baseline_realtime =
        crate::os::signal::Sigaction::query(crate::os::signal::realtime_min()).unwrap();
    let (_directory, library) = native_signal_handler_library();
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        for (export, message) in [
            #[cfg(target_os = "linux")]
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
        let closed_handler = owner.shared().instance.resolve_link_addr(link_addr);
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

    let restored_usr1 = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
    let restored_segv = crate::os::signal::Sigaction::query(crate::os::signal::SIGSEGV).unwrap();
    assert!(restored_usr1.same_disposition(&baseline_usr1));
    assert!(restored_segv.same_disposition(&baseline_segv));
    #[cfg(target_os = "linux")]
    {
        let restored_realtime =
            crate::os::signal::Sigaction::query(crate::os::signal::realtime_min()).unwrap();
        assert!(restored_realtime.same_disposition(&baseline_realtime));
    }
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
    let (_restore_usr1, baseline_usr1) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let (_restore_usr2, baseline_usr2) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR2);
    let (_directory, library) = native_signal_handler_library();
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let preexisting = raw_sigaction(crate::os::signal::SIG_DFL, &[crate::os::signal::SIGHUP]);
        let preexisting_guard = preexisting
            .block_for_handler(crate::os::signal::SIGWINCH)
            .unwrap();
        let engine = engine(
            native_image_signal_module(&library, LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR)),
            jit,
        );
        let install = unsafe { run_export(&engine, "install", &[]) };
        let image = &engine.shared().instance.native_images[0];
        let trace_address = crate::os::dll::sym(image.handle(), c"read_image_signal_trace");
        assert_ne!(trace_address, 0, "native trace reader was not exported");
        let read_trace: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(trace_address) };

        assert_eq!(
            crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
            0
        );
        wait_for_owner_signal_pending(&engine);
        let before_safe_point = unsafe { read_trace() };
        let safe = unsafe { run_export(&engine, "safe", &[]) };
        let after_safe_point = unsafe { read_trace() };
        let preserved_preexisting_mask = crate::os::signal::Sigaction::current_standard_mask_bits()
            .unwrap()
            & (1u64 << crate::os::signal::SIGHUP)
            != 0;
        drop(preexisting_guard);
        engine.wait_closed().unwrap();
        let restored_usr1 =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        let restored_usr2 =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR2).unwrap();

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
    let (_restore_usr1, baseline_usr1) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let (_restore_usr2, baseline_usr2) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR2);
    let (_directory, library) = native_signal_handler_library();
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        let engine = engine(
            native_image_signal_fault_module(&library, LinkAddr(SIGNAL_IMAGE_NESTED_GUEST_ADDR)),
            jit,
        );
        let install = unsafe { run_export(&engine, "install", &[]) };
        let image = &engine.shared().instance.native_images[0];
        let trace_address = crate::os::dll::sym(image.handle(), c"read_image_signal_trace");
        assert_ne!(trace_address, 0, "native trace reader was not exported");
        let read_trace: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(trace_address) };

        assert_eq!(
            crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
            0
        );
        wait_for_owner_signal_pending(&engine);
        let before_safe_point = unsafe { read_trace() };
        let safe = unsafe { run_export(&engine, "safe", &[]) };
        let after_fault = unsafe { read_trace() };
        engine.wait_closed().unwrap();
        let restored_usr1 =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        let restored_usr2 =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR2).unwrap();

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
