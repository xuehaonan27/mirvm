//! Tests for the deferred-hold owners, split by subject: TSD registration
//! lifetime, the native archive entry points and the pthread start window.
//!
//! Shared statics, fixtures and module builders live here; the siblings
//! reach them through `use super::*`.

mod native;
mod pthread;
mod tsd;

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::super::ctx::{Engine, Shared, activate};
use super::super::ffi::inbound::call_guest_ffi;
use super::super::interp::{RunOutcome, run_export};
use super::super::ir::{
    Block, FfiKind, ForeignSig, FuncBody, LinkAddr, MemOrd, Module, Operand, ParamAbi, PlaceBase,
    PlaceExpr, PlaceStep, RetAbi, RetDest, RmwOp, Rvalue, ScalarPlace, Slot, Stmt, Terminator,
    UnwindAction, Width,
};
use super::{TSD_DTOR_ROUNDS, TSD_KEYS, TsdRegistration, prepare_pthread_operation};
#[cfg(target_os = "linux")]
use crate::os::signal::{SIGWINCH, send_to_thread};
#[cfg(target_os = "linux")]
use crate::os::thread::current_thread;
use crate::os::thread::{
    TLS_KEY_GONE, ThreadId, TlsKey, join_raw, tls_get, tls_key_create_raw, tls_key_delete, tls_set,
};

const TSD_ENTRY: u64 = 0xde33_7000;

fn sig(args: Vec<FfiKind>, ret: FfiKind, thunk_args: Vec<(usize, ForeignSig)>) -> ForeignSig {
    ForeignSig {
        args,
        ret,
        fixed: None,
        thunk_args,
        unwind: false,
    }
}

fn engine(module: Module, jit: bool) -> Engine {
    #[allow(unused_mut)]
    let mut shared = Shared::new(module);
    #[cfg(feature = "cranelift")]
    if jit {
        shared.jit.enabled = true;
        shared.jit.threshold = 1;
        shared.jit.sync = true;
    }
    #[cfg(not(feature = "cranelift"))]
    assert!(!jit);
    Engine::new(shared)
}

fn jit_modes() -> Vec<bool> {
    #[allow(unused_mut)]
    let mut modes = vec![false];
    #[cfg(feature = "cranelift")]
    modes.push(true);
    modes
}

fn tsd_module(marker: &AtomicU64, reinstall: bool) -> Module {
    let status = Slot {
        off: 0,
        width: Width::W32,
    };
    let key_ptr = Slot {
        off: 8,
        width: Width::W64,
    };
    let key_value = PlaceExpr {
        base: PlaceBase::Local(key_ptr.off),
        steps: Box::new([PlaceStep::Deref]),
    };
    let dtor_sig = sig(vec![FfiKind::Ptr], FfiKind::Void, Vec::new());
    let register = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(key_ptr)],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallForeign {
                    sym: "pthread_key_create".into(),
                    sig: sig(
                        vec![FfiKind::Ptr, FfiKind::Ptr],
                        FfiKind::I32,
                        vec![(1, dtor_sig)],
                    ),
                    args: vec![
                        Operand::Slot(key_ptr),
                        Operand::Imm {
                            bits: TSD_ENTRY,
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
                term: Terminator::CallForeign {
                    sym: "pthread_setspecific".into(),
                    sig: sig(vec![FfiKind::U32, FfiKind::Ptr], FfiKind::I32, Vec::new()),
                    args: vec![
                        Operand::Mem {
                            expr: key_value,
                            width: Width::W32,
                        },
                        Operand::Slot(key_ptr),
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(status)),
                    target: 2,
                    unwind: UnwindAction::Terminate,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "register_tsd_dtor".into(),
    };
    let value = Slot {
        off: 0,
        width: Width::W64,
    };
    let scratch = Slot {
        off: 8,
        width: Width::W64,
    };
    let mut dtor_blocks = vec![Block {
        stmts: vec![Stmt::AtomicRmw {
            op: RmwOp::Add,
            addr: Operand::Imm {
                bits: marker.as_ptr() as u64,
                width: Width::W64,
            },
            val: Operand::Imm {
                bits: 1,
                width: Width::W64,
            },
            dst: ScalarPlace::Slot(scratch),
            order: MemOrd::SeqCst,
        }],
        term: if reinstall {
            Terminator::CallForeign {
                sym: "pthread_setspecific".into(),
                sig: sig(vec![FfiKind::U32, FfiKind::Ptr], FfiKind::I32, Vec::new()),
                args: vec![
                    Operand::Mem {
                        expr: PlaceExpr {
                            base: PlaceBase::Local(value.off),
                            steps: Box::new([PlaceStep::Deref]),
                        },
                        width: Width::W32,
                    },
                    Operand::Slot(value),
                ],
                ret: RetDest::Scalar(ScalarPlace::Slot(Slot {
                    off: 16,
                    width: Width::W32,
                })),
                target: 1,
                unwind: UnwindAction::Terminate,
            }
        } else {
            Terminator::Return
        },
    }];
    if reinstall {
        dtor_blocks.push(Block {
            stmts: Vec::new(),
            term: Terminator::Return,
        });
    }
    let dtor = FuncBody {
        frame_size: 24,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(value)],
        caller_loc_off: None,
        blocks: dtor_blocks,
        name: "guest_tsd_dtor".into(),
    };
    let mut module = Module {
        funcs: vec![register, dtor].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_entry_links.push((LinkAddr(TSD_ENTRY), 1));
    module
}

fn build_native_key_delete_archive() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("mirvm-tsd-native-delete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("delete.c");
    let object = dir.join("delete.o");
    let archive = dir.join("libdelete.a");
    std::fs::write(
            &source,
            concat!(
                "#include <pthread.h>\n",
                "int mirvm_delete_key_in_native(pthread_key_t key) { return pthread_key_delete(key); }\n",
                "int mirvm_set_key_in_native(pthread_key_t key, const void *value) { return pthread_setspecific(key, value); }\n",
                "int mirvm_create_key_in_native(pthread_key_t *key, void (*dtor)(void *)) { return pthread_key_create(key, dtor); }\n",
                "static void *(*saved_start)(void *);\n",
                "static void *saved_arg;\n",
                "static void *delayed_start(void *gate) { while (!__atomic_load_n((unsigned long *)gate, __ATOMIC_ACQUIRE)) {} return saved_start(saved_arg); }\n",
                "int mirvm_create_thread_in_native(pthread_t *thread, const pthread_attr_t *attr, void *(*start)(void *), void *gate) { saved_start = start; saved_arg = gate; return pthread_create(thread, attr, delayed_start, gate); }\n",
            ),
        )
        .unwrap();
    assert!(
        std::process::Command::new("cc")
            .args(["-fPIC", "-c"])
            .arg(&source)
            .arg("-o")
            .arg(&object)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("ar")
            .arg("crs")
            .arg(&archive)
            .arg(&object)
            .status()
            .unwrap()
            .success()
    );
    let library = crate::native::archive::materialize_in(&archive, &dir.join("cache")).unwrap();
    (dir, library)
}

fn run_child(name: &str, env: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(env, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
