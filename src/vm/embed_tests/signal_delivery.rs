//! Signal delivery tests: masks, target-thread delivery, sigwaitinfo and
//! `wait_closed` while a signal is pending.

use super::*;

static SIGNAL_OVERRIDE_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_EXTERNAL_SIGINFO_CODE: AtomicI32 = AtomicI32::new(i32::MIN);
static SIGNAL_EXTERNAL_WAIT_ENTERED: AtomicU64 = AtomicU64::new(0);
static SIGNAL_EXTERNAL_WAIT_RELEASED: AtomicU64 = AtomicU64::new(0);
static SIGNAL_TARGET_HANDLER_RAN: AtomicU64 = AtomicU64::new(0);
static SIGNAL_TARGET_HANDLER_THREAD: AtomicU64 = AtomicU64::new(0);
static SIGNAL_TARGET_WORKER_THREAD: AtomicU64 = AtomicU64::new(0);
const SIGNAL_TARGET_GUEST_ADDR: u64 = 0xe23a;

unsafe extern "C-unwind" fn signal_target_blocking_entry() {
    SIGNAL_TARGET_WORKER_THREAD.store(
        crate::os::thread::current_thread().as_u64(),
        Ordering::SeqCst,
    );
    unsafe { lifecycle_blocking_entry() };
}

unsafe extern "C-unwind" fn record_signal_target_handler_thread() {
    SIGNAL_TARGET_HANDLER_THREAD.store(
        crate::os::thread::current_thread().as_u64(),
        Ordering::SeqCst,
    );
    SIGNAL_TARGET_HANDLER_RAN.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn external_siginfo_signal(
    _signum: i32,
    info: crate::os::signal::SignalInfo,
    _context: *mut std::ffi::c_void,
) {
    let code = crate::os::signal::info_code(info);
    SIGNAL_EXTERNAL_SIGINFO_CODE.store(code, Ordering::SeqCst);
}

unsafe extern "C" fn external_siginfo_wait_for_replacement(
    _signum: i32,
    _info: crate::os::signal::SignalInfo,
    _context: *mut std::ffi::c_void,
) {
    SIGNAL_EXTERNAL_WAIT_ENTERED.store(1, Ordering::SeqCst);
    while SIGNAL_EXTERNAL_WAIT_RELEASED.load(Ordering::SeqCst) == 0 {
        std::hint::spin_loop();
    }
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

fn target_thread_signal_module() -> Module {
    let handler = function(
        "record_target_thread_signal_handler",
        1,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: record_signal_target_handler_thread as *const () as usize as u64,
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
    let safe = function(
        "target_thread_signal_safe_point",
        0,
        RetAbi::Zst,
        Terminator::Return,
    );
    let raise = function(
        "target_thread_wrapped_raise",
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
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let block = calls_native(
        "target_thread_blocks_inside_native_call",
        signal_target_blocking_entry,
    );
    let mut module = Module {
        funcs: vec![handler, safe, raise, block].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 1);
    module.exports.insert("raise".into(), 2);
    module.exports.insert("block".into(), 3);
    module.fn_addrs.insert(SIGNAL_TARGET_GUEST_ADDR, 0);
    module
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
            role: super::ir::BuiltinCallRole::Normal,
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
        role: super::ir::BuiltinCallRole::Normal,
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
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 1);
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
                    role: super::ir::BuiltinCallRole::Normal,
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
                    role: super::ir::BuiltinCallRole::Normal,
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
    module.fn_addrs.insert(SIGNAL_OVERRIDE_GUEST_ADDR, 0);
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
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, query].into(),
        ..Module::default()
    };
    module.exports.insert("query".into(), 1);
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 0);
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
            role: super::ir::BuiltinCallRole::Normal,
        },
    );
    let mut module = Module {
        funcs: vec![handler, sigaction].into(),
        ..Module::default()
    };
    module.exports.insert("sigaction".into(), 1);
    module.fn_addrs.insert(SIGNAL_OWNER_GUEST_ADDR, 0);
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
        let image = &engine.shared().module.native_images[0];
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
            crate::os::signal::SI_TKILL,
            "native finalizer changed libc raise siginfo provenance"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::physically_blocked_native_finalizer_raise_survives_close_worker_exit";
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
            .module
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
        while !super::signal::has_engine_pending(owner.control())
            && !super::signal::has_engine_pending(installer.control())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "cross-Engine signal was not published to either Engine inbox"
            );
            std::thread::yield_now();
        }
        let owner_received = super::signal::has_engine_pending(owner.control());
        let installer_was_idle = !super::signal::has_engine_pending(installer.control());
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
            .module
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
        super::signal::install_signal(
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
            super::signal::native_sigaction(
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
    assert_eq!(native_code, crate::os::signal::SI_TKILL);

    let mut failures = Vec::new();
    for (mode, jit) in modes() {
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
            || wrapped_code != native_code
        {
            failures.push(format!(
                "{mode}: raise={raised:?}, before-unblock={before_unblock}, libc-code={native_code}, wrapped-code={wrapped_code}"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "blocked HostRaise changed an external native handler's siginfo provenance: {failures:#?}"
    );
}

#[test]
fn sigwaitinfo_consumes_blocked_host_raise_without_leaving_thread_signal_state() {
    const CHILD: &str = "MIRVM_SIGWAIT_HOST_RAISE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (_mode, jit) in modes() {
            let (_restore, baseline) =
                SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
            SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
            let engine = engine(physically_masked_signal_module(), jit);
            super::signal::install_signal(
                engine.control(),
                crate::os::signal::SIGUSR1,
                SIGNAL_OWNER_GUEST_ADDR as usize,
                Some((0, SIGNAL_OWNER_GUEST_ADDR)),
            )
            .unwrap();

            let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
            let mask_guard = blocker
                .block_for_handler(crate::os::signal::SIGUSR1)
                .unwrap();
            let raised = unsafe { run_export(&engine, "raise", &[]) };

            let waited_set =
                crate::os::signal::SignalMask::empty().with(crate::os::signal::SIGUSR1);
            let raised_pending = crate::os::signal::wait_pending(&waited_set);
            assert_eq!(
                raised_pending.as_ref().map(|pending| pending.signum()),
                Ok(crate::os::signal::SIGUSR1)
            );
            let raised_code = raised_pending.unwrap().code();

            assert_eq!(
                crate::os::signal::send_to_current_thread(crate::os::signal::SIGUSR1),
                0
            );
            let killed_pending = crate::os::signal::wait_pending(&waited_set);
            assert_eq!(
                killed_pending.as_ref().map(|pending| pending.signum()),
                Ok(crate::os::signal::SIGUSR1)
            );
            let killed_code = killed_pending.unwrap().code();
            let safe = unsafe { run_export(&engine, "probe", &[]) };
            let handler_ran = SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst);
            engine.wait_closed().unwrap();
            let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
            drop(mask_guard);

            assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert_eq!(raised_code, crate::os::signal::SI_TKILL);
            assert_eq!(killed_code, crate::os::signal::SI_TKILL);
            assert!(matches!(safe, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert_eq!(
                handler_ran, 0,
                "sigwaitinfo-consumed signals reached the guest callback"
            );
            assert!(restored.same_disposition(&baseline));
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::sigwaitinfo_consumes_blocked_host_raise_without_leaving_thread_signal_state";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start sigwaitinfo HostRaise subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("sigwaitinfo did not consume the blocked HostRaise");
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
        "blocked HostRaise did not preserve sigwaitinfo semantics:\n{stdout}{stderr}"
    );
}

#[test]
fn blocked_host_raise_runs_once_at_the_target_pthreads_next_safe_point() {
    let _serial = SIGNAL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_restore, baseline) =
        SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
    let mut failures = Vec::new();

    for (mode, jit) in modes() {
        SIGNAL_TARGET_HANDLER_RAN.store(0, Ordering::SeqCst);
        SIGNAL_TARGET_HANDLER_THREAD.store(0, Ordering::SeqCst);
        let engine = engine(target_thread_signal_module(), jit);
        super::signal::install_signal(
            engine.control(),
            crate::os::signal::SIGUSR1,
            SIGNAL_TARGET_GUEST_ADDR as usize,
            Some((0, SIGNAL_TARGET_GUEST_ADDR)),
        )
        .unwrap();

        let execution = engine.clone();
        let observed = std::thread::spawn(move || {
            let target_thread = crate::os::thread::current_thread().as_u64();
            let blocker = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
            let mask_guard = blocker
                .block_for_handler(crate::os::signal::SIGUSR1)
                .unwrap();
            let raised = unsafe { run_export(&execution, "raise", &[]) };
            let while_masked = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            drop(mask_guard);
            let after_unblock = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            let first_safe = unsafe { run_export(&execution, "probe", &[]) };
            let after_first_safe = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            let second_safe = unsafe { run_export(&execution, "probe", &[]) };
            let after_second_safe = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            (
                target_thread,
                raised,
                while_masked,
                after_unblock,
                first_safe,
                after_first_safe,
                second_safe,
                after_second_safe,
            )
        })
        .join()
        .expect("target pthread panicked");
        let handler_thread = SIGNAL_TARGET_HANDLER_THREAD.load(Ordering::SeqCst);
        engine.wait_closed().unwrap();
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        if !matches!(observed.1, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || observed.2 != 0
            || observed.3 != 0
            || !matches!(observed.4, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || observed.5 != 1
            || !matches!(observed.6, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || observed.7 != 1
            || handler_thread != observed.0
            || !restored.same_disposition(&baseline)
        {
            failures.push(format!(
                "{mode}: raise={:?}, masked={}, unblocked={}, first={:?}/{}, second={:?}/{}, target={:#x}, handler={handler_thread:#x}, restored={}",
                observed.1,
                observed.2,
                observed.3,
                observed.4,
                observed.5,
                observed.6,
                observed.7,
                observed.0,
                restored.same_disposition(&baseline),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "blocked HostRaise did not stay with its target pthread: {failures:#?}"
    );
}

#[test]
fn raw_pthread_kill_runs_only_on_the_target_pthread_during_concurrent_close() {
    const CHILD: &str = "MIRVM_RAW_PTHREAD_KILL_TARGET_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
        for (_mode, jit) in modes() {
            reset_lifecycle_gate();
            SIGNAL_TARGET_HANDLER_RAN.store(0, Ordering::SeqCst);
            SIGNAL_TARGET_HANDLER_THREAD.store(0, Ordering::SeqCst);
            SIGNAL_TARGET_WORKER_THREAD.store(0, Ordering::SeqCst);
            let engine = engine(target_thread_signal_module(), jit);
            super::signal::install_signal(
                engine.control(),
                crate::os::signal::SIGUSR1,
                SIGNAL_TARGET_GUEST_ADDR as usize,
                Some((0, SIGNAL_TARGET_GUEST_ADDR)),
            )
            .unwrap();

            let execution = engine.clone();
            let running =
                std::thread::spawn(move || unsafe { run_export(&execution, "block", &[]) });
            wait_lifecycle_entry();
            let target_thread = SIGNAL_TARGET_WORKER_THREAD.load(Ordering::SeqCst);
            let sender_thread = crate::os::thread::current_thread().as_u64();
            assert_ne!(target_thread, 0);
            assert_ne!(target_thread, sender_thread);
            assert_eq!(
                crate::os::signal::send_to_thread(
                    crate::os::thread::ThreadId::from_raw(target_thread),
                    crate::os::signal::SIGUSR1,
                ),
                0
            );
            wait_for_owner_signal_pending(&engine);

            let closer = engine.clone();
            let closed = std::thread::spawn(move || closer.wait_closed());
            release_lifecycle_entry();
            let completed = running.join().expect("target pthread panicked");
            let closed = closed.join().expect("close worker panicked");
            let handler_ran = SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst);
            let handler_thread = SIGNAL_TARGET_HANDLER_THREAD.load(Ordering::SeqCst);
            let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

            assert!(
                matches!(completed, Ok(RunOutcome::Returned(value)) if value.lo == 0),
                "target VM call did not return normally: {completed:?}"
            );
            assert!(closed.is_ok(), "concurrent close failed: {closed:?}");
            assert_eq!(handler_ran, 1);
            assert_eq!(handler_thread, target_thread);
            assert_ne!(handler_thread, sender_thread);
            assert!(restored.same_disposition(&baseline));
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::raw_pthread_kill_runs_only_on_the_target_pthread_during_concurrent_close";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start raw pthread_kill subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("raw pthread_kill was not drained by its target pthread");
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
        "raw pthread_kill left its target pthread or stalled close:\n{stdout}{stderr}"
    );
}

#[test]
fn wait_closed_fails_fast_for_a_signal_pending_on_the_current_pthread() {
    const CHILD: &str = "MIRVM_WAIT_CLOSED_TARGET_SIGNAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);

        for (_mode, jit) in modes() {
            reset_lifecycle_gate();
            SIGNAL_TARGET_HANDLER_RAN.store(0, Ordering::SeqCst);
            SIGNAL_TARGET_HANDLER_THREAD.store(0, Ordering::SeqCst);
            let engine = engine(target_thread_signal_module(), jit);
            super::signal::install_signal(
                engine.control(),
                crate::os::signal::SIGUSR1,
                SIGNAL_TARGET_GUEST_ADDR as usize,
                Some((0, SIGNAL_TARGET_GUEST_ADDR)),
            )
            .unwrap();

            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (wait_tx, wait_rx) = std::sync::mpsc::channel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let target_engine = engine.clone();
            let target = std::thread::spawn(move || {
                let attached = unsafe { run_export(&target_engine, "probe", &[]) };
                ready_tx
                    .send((crate::os::thread::current_thread().as_u64(), attached))
                    .unwrap();
                wait_rx.recv().unwrap();
                result_tx.send(target_engine.wait_closed()).unwrap();
            });
            let (target_pthread, attached) = ready_rx.recv().unwrap();
            assert!(matches!(attached, Ok(RunOutcome::Returned(value)) if value.lo == 0));

            let active_engine = engine.clone();
            let active =
                std::thread::spawn(move || unsafe { run_export(&active_engine, "block", &[]) });
            wait_lifecycle_entry();

            let (checked_tx, checked_rx) = std::sync::mpsc::channel();
            let (published_tx, published_rx) = std::sync::mpsc::channel();
            super::ctx::set_wait_closed_check_hook(Box::new(move || {
                checked_tx.send(()).unwrap();
                published_rx.recv().unwrap();
            }));
            wait_tx.send(()).unwrap();
            checked_rx.recv().unwrap();
            assert_eq!(
                crate::os::signal::send_to_thread(
                    crate::os::thread::ThreadId::from_raw(target_pthread),
                    crate::os::signal::SIGUSR1,
                ),
                0
            );
            wait_for_owner_signal_pending(&engine);
            published_tx.send(()).unwrap();

            let wait_result = result_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("wait_closed blocked on its current pthread's target signal");
            assert_eq!(
                wait_result,
                Err(super::ctx::WaitClosedError::ActiveOnCurrentThread)
            );
            target.join().expect("target pthread panicked");
            assert_eq!(SIGNAL_TARGET_HANDLER_RAN.load(Ordering::SeqCst), 1);
            assert_eq!(
                SIGNAL_TARGET_HANDLER_THREAD.load(Ordering::SeqCst),
                target_pthread
            );

            release_lifecycle_entry();
            let active = active.join().expect("active pthread panicked");
            assert!(matches!(active, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            engine.wait_closed().unwrap();
            assert!(
                crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
                    .unwrap()
                    .same_disposition(&baseline)
            );
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::wait_closed_fails_fast_for_a_signal_pending_on_the_current_pthread";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start current-pthread signal wait_closed subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("wait_closed blocked on a target signal owned by its current pthread");
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
        "current-pthread signal wait_closed regression failed:\n{stdout}{stderr}"
    );
}

#[test]
fn external_siginfo_handler_can_wait_for_another_threads_host_signal() {
    const CHILD: &str = "MIRVM_EXTERNAL_HANDLER_HOST_SIGNAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
            .expect("failed to save waiting SA_SIGINFO disposition");
        let _restore = SavedSignalDisposition {
            signum: crate::os::signal::SIGUSR1,
            action: saved,
        };
        let waiting = {
            let mut action = raw_sigaction(
                external_siginfo_wait_for_replacement as *const () as usize,
                &[],
            );
            action.or_flags(crate::os::signal::SA_SIGINFO);
            action
        };
        assert_eq!(waiting.install(crate::os::signal::SIGUSR1), 0);
        let waiting_installed =
            crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

        for (_mode, jit) in modes() {
            SIGNAL_EXTERNAL_WAIT_ENTERED.store(0, Ordering::SeqCst);
            SIGNAL_EXTERNAL_WAIT_RELEASED.store(0, Ordering::SeqCst);
            let raiser = engine(physically_masked_signal_module(), jit);
            let installer = engine(first_external_native_signal_module(), jit);
            let install_execution = installer.clone();
            let replacement = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while SIGNAL_EXTERNAL_WAIT_ENTERED.load(Ordering::SeqCst) == 0 {
                    if std::time::Instant::now() >= deadline {
                        SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                        panic!("external SA_SIGINFO handler was not entered");
                    }
                    std::thread::yield_now();
                }
                let installed = unsafe { run_export(&install_execution, "install", &[]) };
                SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                installed
            });

            let raised = unsafe { run_export(&raiser, "raise", &[]) };
            let installed = replacement
                .join()
                .expect("HostSignal replacement thread panicked");
            installer.wait_closed().unwrap();
            raiser.wait_closed().unwrap();
            let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

            assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert!(
                matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == external_siginfo_wait_for_replacement as *const () as usize as u64),
                "HostSignal returned the wrong old handler: {installed:?}"
            );
            assert!(restored.same_disposition(&waiting_installed));
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::external_siginfo_handler_can_wait_for_another_threads_host_signal";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start external-handler HostSignal subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("HostSignal deadlocked behind the external handler it had to release");
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
        "external handler and HostSignal did not make progress independently:\n{stdout}{stderr}"
    );
}

#[test]
fn external_siginfo_handler_can_wait_for_another_threads_engine_close() {
    const CHILD: &str = "MIRVM_EXTERNAL_HANDLER_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1)
            .expect("failed to save waiting SA_SIGINFO disposition");
        let _restore = SavedSignalDisposition {
            signum: crate::os::signal::SIGUSR1,
            action: saved,
        };
        let waiting = {
            let mut action = raw_sigaction(
                external_siginfo_wait_for_replacement as *const () as usize,
                &[],
            );
            action.or_flags(crate::os::signal::SA_SIGINFO);
            action
        };

        for (_mode, jit) in modes() {
            assert_eq!(waiting.install(crate::os::signal::SIGUSR1), 0);
            let waiting_installed =
                crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
            let owner = engine(first_external_native_signal_module(), jit);
            let installed = unsafe { run_export(&owner, "install", &[]) };
            assert!(
                matches!(installed, Ok(RunOutcome::Returned(value)) if value.lo == external_siginfo_wait_for_replacement as *const () as usize as u64),
                "owner did not replace the waiting handler: {installed:?}"
            );
            assert_eq!(waiting.install(crate::os::signal::SIGUSR1), 0);

            SIGNAL_EXTERNAL_WAIT_ENTERED.store(0, Ordering::SeqCst);
            SIGNAL_EXTERNAL_WAIT_RELEASED.store(0, Ordering::SeqCst);
            let raiser = engine(physically_masked_signal_module(), jit);
            let closing_owner = owner.clone();
            let close = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while SIGNAL_EXTERNAL_WAIT_ENTERED.load(Ordering::SeqCst) == 0 {
                    if std::time::Instant::now() >= deadline {
                        SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                        panic!("external SA_SIGINFO handler was not entered");
                    }
                    std::thread::yield_now();
                }
                let closed = closing_owner.wait_closed();
                SIGNAL_EXTERNAL_WAIT_RELEASED.store(1, Ordering::SeqCst);
                closed
            });

            let raised = unsafe { run_export(&raiser, "raise", &[]) };
            let closed = close.join().expect("Engine close thread panicked");
            raiser.wait_closed().unwrap();
            let current = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();

            assert!(matches!(raised, Ok(RunOutcome::Returned(value)) if value.lo == 0));
            assert!(closed.is_ok(), "owner Engine close failed: {closed:?}");
            assert!(
                current.same_disposition(&waiting_installed),
                "owner close overwrote the raw external handler"
            );
        }
        return;
    }

    let test_name = "vm::embed_tests::signal_delivery::external_siginfo_handler_can_wait_for_another_threads_engine_close";
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start external-handler close subprocess");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Engine close deadlocked behind the external handler waiting for it");
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
        "external handler and Engine close did not make progress independently:\n{stdout}{stderr}"
    );
}
