//! The native archive entry points: a real libc call routed through the tracked wrappers.

use super::*;

const TSD_NATIVE_DELETE_CHILD: &str = "MIRVM_TEST_TSD_NATIVE_DELETE_CHILD";

const TSD_NATIVE_SET_CHILD: &str = "MIRVM_TEST_TSD_NATIVE_SET_CHILD";

const TSD_NATIVE_CREATE_CHILD: &str = "MIRVM_TEST_TSD_NATIVE_CREATE_CHILD";

fn native_delete_module(marker: &AtomicU64, library: &std::path::Path) -> Module {
    let mut module = tsd_module(marker, false);
    let key_ptr = Slot {
        off: 8,
        width: Width::W64,
    };
    let mut funcs = Vec::new();
    module.funcs.drain_into(&mut funcs);
    funcs[0].blocks[2].term = Terminator::CallForeign {
        sym: "mirvm_delete_key_in_native".into(),
        sig: sig(vec![FfiKind::U32], FfiKind::I32, Vec::new()),
        args: vec![Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Local(key_ptr.off),
                steps: Box::new([PlaceStep::Deref]),
            },
            width: Width::W32,
        }],
        ret: RetDest::Scalar(ScalarPlace::Slot(Slot {
            off: 0,
            width: Width::W32,
        })),
        target: 3,
        unwind: UnwindAction::Terminate,
    };
    funcs[0].blocks.push(Block {
        stmts: Vec::new(),
        term: Terminator::Return,
    });
    module.funcs = funcs.into();
    module.required_native_libs = vec![library.to_string_lossy().into_owned().into_boxed_str()];
    module
}

fn native_set_module(marker: &AtomicU64, library: &std::path::Path) -> Module {
    let mut module = tsd_module(marker, false);
    let mut funcs = Vec::new();
    module.funcs.drain_into(&mut funcs);
    let Terminator::CallForeign { sym, .. } = &mut funcs[0].blocks[1].term else {
        unreachable!()
    };
    *sym = "mirvm_set_key_in_native".into();
    module.funcs = funcs.into();
    module.required_native_libs = vec![library.to_string_lossy().into_owned().into_boxed_str()];
    module
}

fn native_create_key_module(marker: &AtomicU64, library: &std::path::Path) -> Module {
    let mut module = tsd_module(marker, false);
    let mut funcs = Vec::new();
    module.funcs.drain_into(&mut funcs);
    let Terminator::CallForeign { sym, .. } = &mut funcs[0].blocks[0].term else {
        unreachable!()
    };
    *sym = "mirvm_create_key_in_native".into();
    module.funcs = funcs.into();
    module.required_native_libs = vec![library.to_string_lossy().into_owned().into_boxed_str()];
    module
}

#[test]
fn native_archive_key_delete_revokes_tracked_registration() {
    const NAME: &str =
        "vm::deferred::tests::native::native_archive_key_delete_revokes_tracked_registration";
    if std::env::var_os(TSD_NATIVE_DELETE_CHILD).is_none() {
        run_child(NAME, TSD_NATIVE_DELETE_CHILD);
        return;
    }
    let (dir, library) = build_native_key_delete_archive();
    let marker = AtomicU64::new(0);
    let mut key = TlsKey::from_raw(0);
    let engine = engine(native_delete_module(&marker, &library), false);
    let outcome = unsafe { run_export(&engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
    assert_eq!(outcome, RunOutcome::Returned(Default::default()));
    assert_eq!(
        unsafe { tls_set(key, std::ptr::dangling_mut()) },
        TLS_KEY_GONE,
        "the native archive wrapper did not actually delete the libc key"
    );
    assert!(
        !TSD_KEYS
            .lock()
            .unwrap()
            .contains_key(&(engine.shared().id, key)),
        "native pthread_key_delete left a stale MIRVM registration"
    );
    engine.close();
    for _ in 0..100 {
        if engine.state() == super::super::super::ctx::EngineState::Closed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(
        engine.state(),
        super::super::super::ctx::EngineState::Closed,
        "native pthread_key_delete was invisible to the registration lifecycle"
    );
    assert_eq!(marker.load(Ordering::SeqCst), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn native_archive_setspecific_is_tracked() {
    const NAME: &str = "vm::deferred::tests::native::native_archive_setspecific_is_tracked";
    if std::env::var_os(TSD_NATIVE_SET_CHILD).is_none() {
        run_child(NAME, TSD_NATIVE_SET_CHILD);
        return;
    }
    let (dir, library) = build_native_key_delete_archive();
    let marker = Arc::new(AtomicU64::new(0));
    let engine = engine(native_set_module(&marker, &library), false);
    let worker_engine = engine.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut key = TlsKey::from_raw(0);
        let outcome =
            unsafe { run_export(&worker_engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
        assert_eq!(outcome, RunOutcome::Returned(Default::default()));
        assert!(!unsafe { tls_get(key) }.is_null());
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
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn native_archive_key_create_registers_guest_destructor() {
    const NAME: &str =
        "vm::deferred::tests::native::native_archive_key_create_registers_guest_destructor";
    if std::env::var_os(TSD_NATIVE_CREATE_CHILD).is_none() {
        run_child(NAME, TSD_NATIVE_CREATE_CHILD);
        return;
    }
    let (dir, library) = build_native_key_delete_archive();
    let marker = Arc::new(AtomicU64::new(0));
    let engine = engine(native_create_key_module(&marker, &library), false);
    let worker_engine = engine.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut key = TlsKey::from_raw(0);
        let outcome =
            unsafe { run_export(&worker_engine, "probe", &[(&mut key as *mut _) as u64]) }.unwrap();
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
    let _ = std::fs::remove_dir_all(dir);
}
