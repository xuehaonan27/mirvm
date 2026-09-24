//! A physically blocked signal: what a wrapped raise records and what close restores.

use super::*;

static SIGNAL_EXTERNAL_SIGINFO_CODE: AtomicI32 = AtomicI32::new(i32::MIN);

unsafe extern "C" fn external_siginfo_signal(
    _signum: i32,
    info: crate::os::signal::SignalInfo,
    _context: *mut std::ffi::c_void,
) {
    let code = crate::os::signal::info_code(info);
    SIGNAL_EXTERNAL_SIGINFO_CODE.store(code, Ordering::SeqCst);
}

fn wait_for_external_siginfo_code() -> i32 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let code = SIGNAL_EXTERNAL_SIGINFO_CODE.load(Ordering::SeqCst);
        if code != i32::MIN {
            return code;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "external SA_SIGINFO handler did not observe the signal"
        );
        std::thread::yield_now();
    }
}

fn native_image_finalizer_raise_module(library: &Path) -> Module {
    let arm = function(
        "arm_native_image_finalizer_raise",
        0,
        RetAbi::Zst,
        Terminator::CallForeign {
            sym: "arm_image_finalizer_raise".into(),
            sig: ForeignSig {
                args: vec![FfiKind::I32],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: Vec::new(),
                unwind: false,
            },
            args: vec![Operand::Imm {
                bits: crate::os::signal::SIGUSR1 as u64,
                width: Width::W32,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
        },
    );
    let mut module = Module {
        funcs: vec![arm].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.exports.insert("arm".into(), 0);
    module
}

fn sigaction_mask_module() -> Module {
    let mut handler = function("sigaction_mask_handler", 1, RetAbi::Zst, Terminator::Return);
    handler.blocks[0]
        .stmts
        .push(marker_store(&SIGNAL_OWNER_HANDLER_RAN));
    let query = function(
        "query_sigaction_mask",
        1,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::HostSigaction,
            args: vec![
                Operand::Imm {
                    bits: crate::os::signal::SIGUSR1 as u64,
                    width: Width::W32,
                },
                Operand::Imm {
                    bits: 0,
                    width: Width::W64,
                },
                Operand::Slot(word(8)),
            ],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: super::super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, query].into(),
        ..Module::default()
    };
    module.exports.insert("query".into(), 1);
    module
        .fn_entry_links
        .push((LinkAddr(SIGNAL_OWNER_GUEST_ADDR), 0));
    module
}

#[test]
fn physically_blocked_native_finalizer_raise_survives_close_worker_exit() {
    const CHILD: &str = "MIRVM_MASKED_NATIVE_FINALIZER_RAISE_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
            .expect("failed to save finalizer signal disposition");
        let _restore = SavedSignalDisposition {
            signum: crate::os::signal::SIGUSR1,
            action: saved,
        };
        let external = {
            let mut action = raw_sigaction(external_siginfo_signal as *const () as usize, &[]);
            action.or_flags(crate::os::signal::SA_SIGINFO);
            action
        };
        assert_eq!(external.install(crate::os::signal::SIGUSR1), 0);
        let baseline = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        let (_directory, library) = native_signal_handler_library();
        SIGNAL_EXTERNAL_SIGINFO_CODE.store(i32::MIN, Ordering::SeqCst);
        let engine = engine(native_image_finalizer_raise_module(&library), mode == "jit");
        let image = &engine.shared().instance.native_images[0];
        let trace_address = crate::os::dll::sym(image.handle(), c"read_image_signal_trace");
        assert_ne!(trace_address, 0, "native trace reader was not exported");
        let read_trace: unsafe extern "C" fn() -> u64 =
            unsafe { std::mem::transmute(trace_address) };

        let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
        let mask_guard = blocker
            .block_for_handler(crate::os::signal::SIGUSR1)
            .unwrap();
        let armed = unsafe { run_export(&engine, "arm", &[]) };
        engine.wait_closed().unwrap();
        let trace = unsafe { read_trace() };
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        drop(mask_guard);
        let native_code = wait_for_external_siginfo_code();

        assert!(matches!(armed, Ok(RunOutcome::Returned(value)) if value.lo == 0));
        assert_eq!(
            trace, 78,
            "native finalizer did not execute a successful wrapped raise"
        );
        assert_eq!(
            native_code,
            crate::os::signal::RAISE_DELIVERY_CODE,
            "native finalizer changed libc raise siginfo provenance"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::mask::physically_blocked_native_finalizer_raise_survives_close_worker_exit";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, mode)
            .output()
            .expect("failed to start masked native-finalizer subprocess");
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
        "native finalizer raise was lost with its close worker: {failures:#?}"
    );
}

#[test]
fn sigaction_normalizes_uncatchable_mask_bits_and_close_restores_baseline() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        let engine = engine(sigaction_mask_module(), jit);
        let mut action = raw_sigaction(SIGNAL_OWNER_GUEST_ADDR as usize, &[]);
        action.fill_mask();
        let mut old = crate::os::signal::Sigaction::empty(crate::os::signal::SIG_DFL, 0);
        let install = unsafe {
            super::super::signal::native_sigaction(
                crate::os::signal::SIGUSR1,
                std::ptr::from_ref(&action),
                std::ptr::from_mut(&mut old),
                engine.control().id(),
            )
        };
        let mut queried = crate::os::signal::Sigaction::empty(crate::os::signal::SIG_DFL, 0);
        let query =
            unsafe { run_export(&engine, "query", &[std::ptr::from_mut(&mut queried) as u64]) };
        assert_eq!(
            crate::os::signal::kill(crate::os::process::getpid(), crate::os::signal::SIGUSR1),
            0
        );
        wait_for_owner_signal_pending(&engine);
        let safe_point = unsafe { run_export(&engine, "query", &[0]) };
        let kill_masked = queried.mask_contains(crate::os::signal::SIGKILL) as i32;
        let stop_masked = queried.mask_contains(crate::os::signal::SIGSTOP) as i32;
        let usr2_masked = queried.mask_contains(crate::os::signal::SIGUSR2) as i32;
        let handler_ran = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        if install != 0
            || !matches!(query, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || !matches!(safe_point, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || old.handler() != baseline.handler()
            || queried.handler() != SIGNAL_OWNER_GUEST_ADDR as usize
            || kill_masked != 0
            || stop_masked != 0
            || usr2_masked != 1
            || handler_ran != 1
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: install={install}, query={query:?}, kill-mask={kill_masked}, stop-mask={stop_masked}, usr2-mask={usr2_masked}, handler={handler_ran}, restored={}",
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "sigaction did not normalize SIGKILL/SIGSTOP mask bits: {failures:#?}"
    );
}

#[test]
fn blocked_external_native_raise_keeps_libc_si_tkill_provenance() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let saved = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
        .expect("failed to save external SA_SIGINFO test disposition");
    let _restore = SavedSignalDisposition {
        signum: crate::os::signal::SIGUSR1,
        action: saved,
    };
    let external = {
        let mut action = raw_sigaction(external_siginfo_signal as *const () as usize, &[]);
        action.or_flags(crate::os::signal::SA_SIGINFO);
        action
    };
    assert_eq!(external.install(crate::os::signal::SIGUSR1), 0);

    let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
    SIGNAL_EXTERNAL_SIGINFO_CODE.store(i32::MIN, Ordering::SeqCst);
    let native_guard = blocker
        .block_for_handler(crate::os::signal::SIGUSR1)
        .unwrap();
    assert_eq!(crate::os::process::raise(crate::os::signal::SIGUSR1), 0);
    assert_eq!(
        SIGNAL_EXTERNAL_SIGINFO_CODE.load(Ordering::SeqCst),
        i32::MIN
    );
    drop(native_guard);
    let native_code = wait_for_external_siginfo_code();
    assert_eq!(native_code, crate::os::signal::RAISE_DELIVERY_CODE);

    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        // Which `si_code` this kernel puts on a raise that waited behind a blocked mask depends on
        // process history and not only on the sender: measured on the macos aarch64 host, the first
        // such raise after this process spawned a child arrives with code 1 and the sending pid,
        // and every later one with code 0 and no sender. Building an Engine runs this platform's
        // signer, which is a child process. The libc baseline is therefore taken right after a
        // build of its own, so that both raises below are the first after one, and the wrapped
        // raise happens in the Engine built after the baseline rather than the one that timed it.
        let baseline_engine = engine(physically_masked_signal_module(), jit);
        SIGNAL_EXTERNAL_SIGINFO_CODE.store(i32::MIN, Ordering::SeqCst);
        let baseline_guard = blocker
            .block_for_handler(crate::os::signal::SIGUSR1)
            .unwrap();
        assert_eq!(crate::os::process::raise(crate::os::signal::SIGUSR1), 0);
        drop(baseline_guard);
        let libc_code = wait_for_external_siginfo_code();
        baseline_engine.wait_closed().unwrap();

        SIGNAL_EXTERNAL_SIGINFO_CODE.store(i32::MIN, Ordering::SeqCst);
        let engine = engine(physically_masked_signal_module(), jit);
        let wrapped_guard = blocker
            .block_for_handler(crate::os::signal::SIGUSR1)
            .unwrap();
        let raised = unsafe { run_export(&engine, "raise", &[]) };
        let before_unblock = SIGNAL_EXTERNAL_SIGINFO_CODE.load(Ordering::SeqCst);
        drop(wrapped_guard);
        let wrapped_code = wait_for_external_siginfo_code();
        engine.wait_closed().unwrap();

        if !matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || before_unblock != i32::MIN
            || wrapped_code != libc_code
        {
            failures.push(format!(
                "{mode}: raise={raised:?}, before-unblock={before_unblock}, libc-code={libc_code}, wrapped-code={wrapped_code}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "blocked HostRaise changed an external native handler's siginfo provenance: {failures:#?}"
    );
}
