//! The pthread start window: the hold that covers create until the start routine runs.

use super::*;

const START_CHILD: &str = "MIRVM_TEST_PTHREAD_START_CHILD";

const START_NATIVE_CREATE_CHILD: &str = "MIRVM_TEST_PTHREAD_NATIVE_CREATE_CHILD";

const START_ENTRY: u64 = 0xde33_7100;

fn native_start_module(
    thread: *mut ThreadId,
    marker: &AtomicU64,
    release: &AtomicU64,
    library: &std::path::Path,
) -> Module {
    let mut module = start_module(thread, marker);
    let mut funcs = Vec::new();
    module.funcs.drain_into(&mut funcs);
    let Terminator::CallForeign { sym, args, .. } = &mut funcs[0].blocks[0].term else {
        unreachable!()
    };
    *sym = "mirvm_create_thread_in_native".into();
    args[3] = Operand::Imm {
        bits: release.as_ptr() as u64,
        width: Width::W64,
    };
    module.funcs = funcs.into();
    module.required_native_libs = vec![library.to_string_lossy().into_owned().into_boxed_str()];
    module
}

fn start_module(thread: *mut ThreadId, marker: &AtomicU64) -> Module {
    let status = Slot {
        off: 0,
        width: Width::W32,
    };
    let start_sig = sig(vec![FfiKind::Ptr], FfiKind::Ptr, Vec::new());
    let spawn = FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallForeign {
                    sym: "pthread_create".into(),
                    sig: sig(
                        vec![FfiKind::Ptr, FfiKind::Ptr, FfiKind::Ptr, FfiKind::Ptr],
                        FfiKind::I32,
                        vec![(2, start_sig)],
                    ),
                    args: vec![
                        Operand::Imm {
                            bits: thread as u64,
                            width: Width::W64,
                        },
                        Operand::Imm {
                            bits: 0,
                            width: Width::W64,
                        },
                        Operand::Imm {
                            bits: START_ENTRY,
                            width: Width::W64,
                        },
                        Operand::Imm {
                            bits: 0,
                            width: Width::W64,
                        },
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(status)),
                    target: 1,
                    unwind: UnwindAction::Terminate,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "spawn_guest_thread".into(),
    };
    let ret = Slot {
        off: 0,
        width: Width::W64,
    };
    let start = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Scalar(ret),
        params: vec![ParamAbi::Scalar(Slot {
            off: 8,
            width: Width::W64,
        })],
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![
                Stmt::AtomicStore {
                    addr: Operand::Imm {
                        bits: marker.as_ptr() as u64,
                        width: Width::W64,
                    },
                    val: Operand::Imm {
                        bits: 1,
                        width: Width::W64,
                    },
                    order: super::super::super::ir::MemOrd::SeqCst,
                },
                Stmt::Assign {
                    dst: ScalarPlace::Slot(ret),
                    rv: Rvalue::Use(Operand::Imm {
                        bits: 0,
                        width: Width::W64,
                    }),
                },
            ],
            term: Terminator::Return,
        }],
        name: "guest_thread_start".into(),
    };
    let mut module = Module {
        funcs: vec![spawn, start].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_entry_links.push((LinkAddr(START_ENTRY), 1));
    module
}

#[test]
fn native_archive_pthread_create_holds_until_delayed_start() {
    const NAME: &str =
        "vm::deferred::tests::pthread::native_archive_pthread_create_holds_until_delayed_start";
    if std::env::var_os(START_NATIVE_CREATE_CHILD).is_none() {
        run_child(NAME, START_NATIVE_CREATE_CHILD);
        return;
    }
    let (dir, library) = build_native_key_delete_archive();
    let marker = AtomicU64::new(0);
    let release = AtomicU64::new(0);
    let mut thread = ThreadId::from_raw(0);
    let engine = engine(
        native_start_module(&mut thread, &marker, &release, &library),
        false,
    );
    let outcome = unsafe { run_export(&engine, "probe", &[]) }.unwrap();
    assert_eq!(outcome, RunOutcome::Returned(Default::default()));
    engine.close();
    assert_eq!(
        engine.state(),
        super::super::super::ctx::EngineState::Closing
    );
    assert_eq!(marker.load(Ordering::SeqCst), 0);
    release.store(1, Ordering::Release);
    join_raw(thread);
    engine.wait_closed().unwrap();
    assert_eq!(marker.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pthread_create_start_hold_closes_the_start_window() {
    const NAME: &str =
        "vm::deferred::tests::pthread::pthread_create_start_hold_closes_the_start_window";
    if std::env::var_os(START_CHILD).is_none() {
        run_child(NAME, START_CHILD);
        return;
    }
    for jit in jit_modes() {
        for _ in 0..64 {
            let marker = AtomicU64::new(0);
            let mut thread = ThreadId::from_raw(0);
            let engine = engine(start_module(&mut thread, &marker), jit);
            let outcome = unsafe { run_export(&engine, "probe", &[]) }.unwrap();
            assert_eq!(outcome, RunOutcome::Returned(Default::default()));
            engine.wait_closed().unwrap();
            join_raw(thread);
            assert_eq!(marker.load(Ordering::SeqCst), 1);
        }
    }
}
