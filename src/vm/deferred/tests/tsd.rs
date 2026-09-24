//! TSD registration lifetime: destructor rounds, commit/delete operations and thread exit.

use super::*;

const TSD_CHILD: &str = "MIRVM_TEST_TSD_DEFERRED_CHILD";

const TSD_REMOTE_CHILD: &str = "MIRVM_TEST_TSD_REMOTE_CHILD";

const TSD_TOMBSTONE_CHILD: &str = "MIRVM_TEST_TSD_TOMBSTONE_CHILD";

#[cfg(target_os = "linux")]
const EXIT_SIGNAL_TSD_CHILD: &str = "MIRVM_TEST_EXIT_SIGNAL_TSD_CHILD";

#[cfg(target_os = "linux")]
const EXIT_SIGNAL_ENTRY: u64 = 0xde33_7200;

#[cfg(target_os = "linux")]
static EXIT_SIGNAL_TSD_KEY: AtomicU64 = AtomicU64::new(u64::MAX);

#[cfg(target_os = "linux")]
static EXIT_SIGNAL_TSD_OWNER: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "linux")]
static EXIT_SIGNAL_TSD_SET_RESULT: AtomicU64 = AtomicU64::new(u64::MAX);

#[cfg(target_os = "linux")]
unsafe extern "C-unwind" fn set_tsd_from_exit_signal() {
    let result = unsafe {
        super::super::native_pthread_setspecific(
            TlsKey::from_raw(EXIT_SIGNAL_TSD_KEY.load(Ordering::SeqCst) as _),
            std::ptr::dangling::<c_void>(),
            EXIT_SIGNAL_TSD_OWNER.load(Ordering::SeqCst),
        )
    };
    EXIT_SIGNAL_TSD_SET_RESULT.store(result as u64, Ordering::SeqCst);
}

#[cfg(target_os = "linux")]
fn exit_signal_tsd_module(marker: &AtomicU64) -> Module {
    let mut module = tsd_module(marker, false);
    let attach = FuncBody {
        frame_size: 0,
        frame_align: 1,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: Vec::new(),
            term: Terminator::Return,
        }],
        name: "attach_before_reusing_low_tsd_key".into(),
    };
    let handler = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(Slot {
            off: 8,
            width: Width::W32,
        })],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: set_tsd_from_exit_signal as *const () as usize as u64,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Continue,
                    null_ok: false,
                    native_sig: Some(sig(Vec::new(), FfiKind::Void, Vec::new())),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "set_managed_tsd_from_exit_signal".into(),
    };
    let attach_id = module.funcs.len() as u32;
    module.funcs.push(attach);
    let handler_id = module.funcs.len() as u32;
    module.funcs.push(handler);
    module.exports.insert("attach".into(), attach_id);
    module
        .fn_entry_links
        .push((LinkAddr(EXIT_SIGNAL_ENTRY), handler_id));
    module
}

fn raw_tsd_registration(engine: &Engine) -> (Arc<TsdRegistration>, crate::os::thread::TlsKey) {
    let registration = TsdRegistration::pending(engine.control()).unwrap();
    let key = crate::os::thread::tls_key_create(None);
    registration.commit(key);
    (registration, key)
}

#[test]
fn wait_closed_drains_current_thread_tsd_dtor() {
    const NAME: &str = "vm::deferred::tests::tsd::wait_closed_drains_current_thread_tsd_dtor";
    if std::env::var_os(TSD_CHILD).is_none() {
        run_child(NAME, TSD_CHILD);
        return;
    }
    for jit in jit_modes() {
        let marker = AtomicU64::new(0);
        let mut key = TlsKey::from_raw(0);
        let engine = engine(tsd_module(&marker, false), jit);
        let outcome =
            unsafe { run_export(&engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
        assert_eq!(outcome, RunOutcome::Returned(Default::default()));
        engine.wait_closed().unwrap();
        assert_eq!(marker.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn close_inside_reinstalling_tsd_dtor_runs_four_rounds() {
    for jit in jit_modes() {
        let marker = AtomicU64::new(0);
        let mut key = TlsKey::from_raw(0);
        let engine = engine(tsd_module(&marker, true), jit);
        let outcome =
            unsafe { run_export(&engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
        assert_eq!(outcome, RunOutcome::Returned(Default::default()));

        let registration = {
            TSD_KEYS
                .lock()
                .unwrap()
                .get(&(engine.shared().id, key))
                .cloned()
                .unwrap()
        };
        unsafe { tls_set(key, std::ptr::null()) };
        let (lease, callback) = registration.enter_callback().unwrap();
        let activation = activate(lease.shared());
        call_guest_ffi(
            activation.ctx(),
            1,
            &[FfiKind::Ptr],
            &[(&mut key as *mut _) as u64],
            None,
        );
        engine.close();
        drop(activation);
        drop(lease);
        drop(callback);

        engine.wait_closed().unwrap();
        assert_eq!(marker.load(Ordering::SeqCst), TSD_DTOR_ROUNDS as u64);
    }
}

/// The scenario needs a signal to reach a thread that is already inside its final Ctx destructor
/// round, and this platform cannot deliver one there at all: measured, `pthread_kill` answers
/// ESRCH both for another thread's attempt on it and for its own, because this kernel retires a
/// thread's identity before running its TSD destructors. So the interleaving cannot be produced,
/// and a signal that cannot arrive in that window is not one there is anything to lose.
/// Everything the round does with a signal that did arrive is the rest of this module's coverage.
#[cfg(all(test, target_os = "linux"))]
#[test]
fn final_ctx_destructor_round_drains_tsd_reset_by_target_signal_callback() {
    const NAME: &str = "vm::deferred::tests::tsd::final_ctx_destructor_round_drains_tsd_reset_by_target_signal_callback";
    if std::env::var_os(EXIT_SIGNAL_TSD_CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(EXIT_SIGNAL_TSD_CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("thread-exit signal left a managed TSD hold behind");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take().unwrap(), &mut stdout).unwrap();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
        assert!(
            status.success(),
            "thread-exit signal did not finish managed TSD teardown:\n{stdout}{stderr}"
        );
        return;
    }

    let signum = SIGWINCH;
    let baseline = crate::os::signal::Sigaction::query(signum).unwrap();
    for jit in jit_modes() {
        for low_slot in [true, false] {
            let low_key = if low_slot {
                let mut key = TlsKey::from_raw(0);
                assert_eq!(unsafe { tls_key_create_raw(&mut key, None) }, 0);
                Some(key)
            } else {
                None
            };

            let marker = AtomicU64::new(0);
            EXIT_SIGNAL_TSD_KEY.store(u64::MAX, Ordering::SeqCst);
            EXIT_SIGNAL_TSD_SET_RESULT.store(u64::MAX, Ordering::SeqCst);
            let engine = engine(exit_signal_tsd_module(&marker), jit);
            let ctx_key = super::super::super::ctx::test_ctx_key();
            EXIT_SIGNAL_TSD_OWNER.store(engine.control().id(), Ordering::SeqCst);
            super::super::super::signal::install_signal(
                engine.control(),
                signum,
                EXIT_SIGNAL_ENTRY as usize,
                Some((3, EXIT_SIGNAL_ENTRY)),
            )
            .unwrap();

            let (attached_tx, attached_rx) = std::sync::mpsc::channel();
            let (reuse_tx, reuse_rx) = std::sync::mpsc::channel();
            let (registered_tx, registered_rx) = std::sync::mpsc::channel();
            let (exit_tx, exit_rx) = std::sync::mpsc::channel();
            let worker_engine = engine.clone();
            let worker = std::thread::spawn(move || {
                let mut key = TlsKey::from_raw(u32::MAX as _);
                assert!(matches!(
                    unsafe { run_export(&worker_engine, "attach", &[]) },
                    Ok(RunOutcome::Returned(_))
                ));
                attached_tx.send(current_thread()).unwrap();
                reuse_rx.recv().unwrap();
                let result = unsafe {
                    run_export(
                        &worker_engine,
                        "probe",
                        &[std::ptr::from_mut(&mut key) as u64],
                    )
                };
                EXIT_SIGNAL_TSD_KEY.store(key.as_raw() as u64, Ordering::SeqCst);
                registered_tx.send((key, result)).unwrap();
                exit_rx.recv().unwrap();
            });

            let target = attached_rx.recv().unwrap();
            let mut filler_keys = Vec::new();
            if let Some(low_key) = low_key {
                assert!(low_key.as_raw() < ctx_key.as_raw());
                assert_eq!(tls_key_delete(low_key), 0);
            } else {
                loop {
                    let mut key = TlsKey::from_raw(0);
                    assert_eq!(unsafe { tls_key_create_raw(&mut key, None) }, 0);
                    filler_keys.push(key);
                    if key.as_raw() > ctx_key.as_raw() {
                        break;
                    }
                }
            }
            reuse_tx.send(()).unwrap();
            let (guest_key, registered) = registered_rx.recv().unwrap();
            if let Some(low_key) = low_key {
                assert_eq!(guest_key, low_key, "guest TSD did not reuse the low key");
            } else {
                assert!(
                    guest_key.as_raw() > ctx_key.as_raw(),
                    "guest TSD did not use a high key"
                );
            }
            assert!(matches!(registered, Ok(RunOutcome::Returned(_))));

            let (empty_tx, empty_rx) = std::sync::mpsc::channel();
            let (publish_tx, publish_rx) = std::sync::mpsc::channel();
            super::super::super::ctx::set_thread_exit_inbox_empty_hook(Box::new(move || {
                empty_tx.send(()).unwrap();
                publish_rx.recv().unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while !super::super::super::signal::current_thread_inbox_handle().has_pending() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "target signal did not reach the final Ctx destructor round"
                    );
                    std::thread::yield_now();
                }
            }));
            exit_tx.send(()).unwrap();
            empty_rx.recv().unwrap();
            assert_eq!(send_to_thread(target, signum), 0);
            publish_tx.send(()).unwrap();
            worker.join().unwrap();

            assert_eq!(EXIT_SIGNAL_TSD_SET_RESULT.load(Ordering::SeqCst), 0);
            engine.wait_closed().unwrap();
            assert_eq!(
                marker.load(Ordering::SeqCst),
                if low_slot { 1 } else { 2 },
                "the final pthread pass did not honor its raw-key cursor"
            );
            assert!(
                crate::os::signal::Sigaction::query(signum)
                    .unwrap()
                    .same_disposition(&baseline)
            );
            for key in filler_keys {
                assert_eq!(tls_key_delete(key), 0);
            }
        }
    }
}

#[test]
fn pthread_key_create_commit_linearizes_with_close() {
    for iteration in 0..128 {
        let engine = engine(Module::default(), false);
        let registration = TsdRegistration::pending(engine.control()).unwrap();
        let mut key = TlsKey::from_raw(0);
        assert_eq!(unsafe { tls_key_create_raw(&mut key, None) }, 0);
        let close = if iteration % 2 == 0 {
            // Deterministically cover close scanning an empty registry
            // before pthread_key_create publishes its successful result.
            engine.close();
            None
        } else {
            let gate = Arc::new(std::sync::Barrier::new(2));
            let closer = engine.clone();
            let close_gate = Arc::clone(&gate);
            let close = std::thread::spawn(move || {
                close_gate.wait();
                closer.close();
            });
            gate.wait();
            Some(close)
        };
        registration.commit(key);
        if let Some(close) = close {
            close.join().unwrap();
        }
        engine.wait_closed().unwrap();
        assert!(
            !TSD_KEYS
                .lock()
                .unwrap()
                .contains_key(&(engine.shared().id, key))
        );
    }
}

#[test]
fn pthread_setspecific_operation_blocks_close_revocation() {
    for iteration in 0..128 {
        let engine = engine(Module::default(), false);
        let (_registration, key) = raw_tsd_registration(&engine);
        let lease = engine.execution_lease().unwrap();
        let value = std::ptr::dangling::<c_void>();
        let operation = prepare_pthread_operation(
            engine.shared(),
            "pthread_setspecific",
            &[key.as_raw() as u64, value as u64],
        )
        .unwrap();
        if iteration % 2 == 0 {
            engine.close();
        } else {
            let gate = Arc::new(std::sync::Barrier::new(2));
            let closer = engine.clone();
            let close_gate = Arc::clone(&gate);
            let close = std::thread::spawn(move || {
                close_gate.wait();
                closer.close();
            });
            gate.wait();
            close.join().unwrap();
        }
        assert_eq!(
            engine.state(),
            super::super::super::ctx::EngineState::Closing
        );
        let result = unsafe { tls_set(key, value) };
        assert_eq!(result, 0, "close revoked a key during pthread_setspecific");
        operation.complete(result as u64);

        let clear = prepare_pthread_operation(
            engine.shared(),
            "pthread_setspecific",
            &[key.as_raw() as u64, 0],
        )
        .unwrap();
        let result = unsafe { tls_set(key, std::ptr::null()) };
        assert_eq!(result, 0, "tracked pthread key became invalid before clear");
        clear.complete(result as u64);
        drop(lease);
        engine.wait_closed().unwrap();
    }
}

#[test]
fn pthread_key_delete_operation_blocks_close_revocation() {
    for iteration in 0..128 {
        let engine = engine(Module::default(), false);
        let (_registration, key) = raw_tsd_registration(&engine);
        let lease = engine.execution_lease().unwrap();
        let operation = prepare_pthread_operation(
            engine.shared(),
            "pthread_key_delete",
            &[key.as_raw() as u64],
        )
        .unwrap();
        if iteration % 2 == 0 {
            engine.close();
        } else {
            let gate = Arc::new(std::sync::Barrier::new(2));
            let closer = engine.clone();
            let close_gate = Arc::clone(&gate);
            let close = std::thread::spawn(move || {
                close_gate.wait();
                closer.close();
            });
            gate.wait();
            close.join().unwrap();
        }
        assert_eq!(
            engine.state(),
            super::super::super::ctx::EngineState::Closing
        );
        let result = tls_key_delete(key);
        operation.complete(result as u64);
        drop(lease);
        engine.wait_closed().unwrap();
    }
}

#[test]
fn deleted_tsd_destructor_thunk_is_a_stable_noop() {
    const NAME: &str = "vm::deferred::tests::tsd::deleted_tsd_destructor_thunk_is_a_stable_noop";
    if std::env::var_os(TSD_TOMBSTONE_CHILD).is_none() {
        run_child(NAME, TSD_TOMBSTONE_CHILD);
        return;
    }
    let marker = AtomicU64::new(0);
    let mut key = TlsKey::from_raw(0);
    let engine = engine(tsd_module(&marker, false), false);
    let outcome = unsafe { run_export(&engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
    assert_eq!(outcome, RunOutcome::Returned(Default::default()));
    let code = {
        let registration = TSD_KEYS
            .lock()
            .unwrap()
            .get(&(engine.shared().id, key))
            .cloned()
            .unwrap();
        registration.state.lock().unwrap().code
    };

    engine.wait_closed().unwrap();
    assert_eq!(marker.load(Ordering::SeqCst), 1);
    let callback: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(code as usize) };
    unsafe { callback((&mut key as *mut TlsKey).cast()) };
    assert_eq!(marker.load(Ordering::SeqCst), 1);
}

#[test]
fn remote_tsd_value_keeps_engine_closing_until_thread_exit() {
    const NAME: &str =
        "vm::deferred::tests::tsd::remote_tsd_value_keeps_engine_closing_until_thread_exit";
    if std::env::var_os(TSD_REMOTE_CHILD).is_none() {
        run_child(NAME, TSD_REMOTE_CHILD);
        return;
    }
    for jit in jit_modes() {
        let marker = Arc::new(AtomicU64::new(0));
        let engine = engine(tsd_module(&marker, false), jit);
        let worker_engine = engine.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut key = TlsKey::from_raw(0);
            let outcome =
                unsafe { run_export(&worker_engine, "probe", &[(&mut key as *mut _) as u64]) }
                    .unwrap();
            assert_eq!(outcome, RunOutcome::Returned(Default::default()));
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });

        ready_rx.recv().unwrap();
        engine.close();
        assert_eq!(
            engine.state(),
            super::super::super::ctx::EngineState::Closing
        );
        assert_eq!(marker.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        engine.wait_closed().unwrap();
        assert_eq!(marker.load(Ordering::SeqCst), 1);
    }
}
