#![feature(rustc_private)]

use std::sync::atomic::{AtomicPtr, Ordering};

use mirvm::vm::raw::{
    Block, Builtin, FfiKind, ForeignSig, FuncBody, Module, Operand, ParamAbi, RetAbi, RetDest,
    ScalarPlace, Slot, Terminator, UnwindAction, Width, run_export_raw,
};
use mirvm::vm::{Engine, RunOutcome};

unsafe extern "C-unwind" {
    fn cpp_reset_caught();
    fn cpp_caught_value() -> i32;
    fn cpp_throw_marker();
    fn cpp_call_typed_catch(callback: unsafe extern "C-unwind" fn()) -> i32;
}

static CURRENT: AtomicPtr<Engine> = AtomicPtr::new(std::ptr::null_mut());

fn native_sig() -> ForeignSig {
    ForeignSig {
        args: vec![],
        ret: FfiKind::Void,
        fixed: None,
        thunk_args: vec![],
        unwind: true,
    }
}

fn foreign_call(name: &str, params: Vec<ParamAbi>) -> FuncBody {
    FuncBody {
        frame_size: if params.is_empty() { 0 } else { 8 },
        frame_align: 8,
        ret: RetAbi::Zst,
        params,
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: vec![],
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: cpp_throw_marker as *const () as usize as u64,
                        width: Width::W64,
                    },
                    args: vec![],
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Continue,
                    null_ok: false,
                    native_sig: Some(native_sig()),
                },
            },
            Block {
                stmts: vec![],
                term: Terminator::Return,
            },
        ],
        name: name.into(),
    }
}

fn engine_top_module() -> Module {
    let mut module = Module {
        funcs: vec![foreign_call("embed_engine_top_foreign", vec![])].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module
}

fn guest_catch_module() -> Module {
    let try_addr = 0xe130_0001;
    let catch_addr = 0xe130_0002;
    let result = Slot {
        off: 0,
        width: Width::W32,
    };
    let outer = FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: vec![],
                term: Terminator::CallBuiltin {
                    builtin: Builtin::CatchUnwind,
                    args: vec![
                        Operand::Imm {
                            bits: try_addr,
                            width: Width::W64,
                        },
                        Operand::Imm {
                            bits: 0,
                            width: Width::W64,
                        },
                        Operand::Imm {
                            bits: catch_addr,
                            width: Width::W64,
                        },
                    ],
                    ret: RetDest::Scalar(ScalarPlace::Slot(result)),
                    target: 1,
                    unwind: UnwindAction::Continue,
                    role: mirvm::vm::raw::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: vec![],
                term: Terminator::Return,
            },
        ],
        name: "embed_guest_std_catch".into(),
    };
    let try_body = foreign_call(
        "embed_guest_try_foreign",
        vec![ParamAbi::Scalar(Slot {
            off: 0,
            width: Width::W64,
        })],
    );
    let catch_body = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![
            ParamAbi::Scalar(Slot {
                off: 0,
                width: Width::W64,
            }),
            ParamAbi::Scalar(Slot {
                off: 8,
                width: Width::W64,
            }),
        ],
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: vec![],
            term: Terminator::Return,
        }],
        name: "embed_guest_catch_must_not_run".into(),
    };
    let mut module = Module {
        funcs: vec![outer, try_body, catch_body].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_addrs.insert(try_addr, 1);
    module.fn_addrs.insert(catch_addr, 2);
    module
}

unsafe extern "C-unwind" fn enter_engine() {
    let ptr = CURRENT.load(Ordering::Acquire);
    assert!(!ptr.is_null(), "Engine callback missing Engine state");
    let result = unsafe { run_export_raw(&*ptr, "probe", &[]) };
    eprintln!("unexpected Engine return: {result:?}");
}

fn main() {
    let mode = std::env::args().nth(1).expect("mode");
    unsafe { cpp_reset_caught() };

    match mode.as_str() {
        "engine-top" => {
            let engine = unsafe { Engine::from_module_unchecked(engine_top_module()) }
                .expect("valid engine-top fixture Module");
            CURRENT.store(&engine as *const Engine as *mut Engine, Ordering::Release);
            let result = unsafe { cpp_call_typed_catch(enter_engine) };
            CURRENT.store(std::ptr::null_mut(), Ordering::Release);
            println!("result={result} caught={}", unsafe { cpp_caught_value() });
        }
        "guest-catch" => {
            let engine = unsafe { Engine::from_module_unchecked(guest_catch_module()) }
                .expect("valid guest-catch fixture Module");
            match unsafe { run_export_raw(&engine, "probe", &[]) } {
                Ok(RunOutcome::Returned(_)) => println!("unexpected returned"),
                Ok(RunOutcome::GuestPanic) => println!("unexpected guest panic"),
                Err(error) => println!("unexpected error: {error}"),
            }
        }
        _ => panic!("unknown mode `{mode}`"),
    }
}
