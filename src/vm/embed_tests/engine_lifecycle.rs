//! Engine lifecycle and fault tests: guest panics, engine faults, close
//! ordering and the teardown boundary.

use super::*;

static FOREIGN_CATCH_RAN: AtomicU64 = AtomicU64::new(0);
static FOREIGN_FAULT_CATCH_RAN: AtomicU64 = AtomicU64::new(0);
static FOREIGN_FAULT_CLEANUP_RAN: AtomicU64 = AtomicU64::new(0);
static FOREIGN_CLOSED_CLEANUP_RAN: AtomicU64 = AtomicU64::new(0);
static FOREIGN_FAULT_BOUNDARY_RESULT: AtomicU64 = AtomicU64::new(0);
static SUSPENDED_FAULT_GUEST_CLEANUP_RAN: AtomicU64 = AtomicU64::new(0);
static THUNK_MAIN_CATCH_RESULT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Eq, PartialEq)]
struct HostPanicMarker(u64);

unsafe extern "C-unwind" fn host_panic_entry() {
    std::panic::panic_any(HostPanicMarker(0xe13));
}

unsafe extern "C-unwind" fn thunk_attempt_main_catch() {
    let ctx = super::ctx::current();
    let observed =
        if super::ctx::claim_main_panic_catch(ctx, super::ir::BuiltinCallRole::MainPanicCatcher)
            .is_none()
        {
            1
        } else {
            2
        };
    THUNK_MAIN_CATCH_RESULT.store(observed, Ordering::SeqCst);
}

unsafe extern "C-unwind" fn nested_engine_entry() {
    let engine = NESTED_ENGINE.with(|slot| {
        slot.borrow()
            .as_ref()
            .expect("nested Engine was not installed")
            .clone()
    });
    let observed = match unsafe { run_export(&engine, "probe", &[]) } {
        Err(error) if error.kind == RunErrorKind::EngineFault => 1,
        Err(_) => 2,
        Ok(_) => 3,
    };
    // A correctly owned EngineFault never returns to this callback: it must
    // pass through B's boundary and continue to A's outer boundary.
    FOREIGN_FAULT_BOUNDARY_RESULT.store(observed, Ordering::SeqCst);
}

fn guest_panic_function(name: &str, params: usize) -> FuncBody {
    function(
        name,
        params,
        RetAbi::Zst,
        Terminator::CallBuiltin {
            builtin: Builtin::UnwindRaise,
            args: vec![Operand::Imm {
                bits: 0x4747,
                width: Width::W64,
            }],
            ret: RetDest::Ignore,
            target: 1,
            unwind: UnwindAction::Continue,
            role: crate::vm::ir::BuiltinCallRole::Normal,
        },
    )
}

fn guest_panic_with_cleanup_function() -> FuncBody {
    FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::UnwindRaise,
                    args: vec![Operand::Imm {
                        bits: 0x5151,
                        width: Width::W64,
                    }],
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Cleanup(2),
                    role: crate::vm::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
            Block {
                stmts: vec![marker_store(&SUSPENDED_FAULT_GUEST_CLEANUP_RAN)],
                term: Terminator::Resume,
            },
        ],
        name: "guest_panic_while_engine_fault_is_suspended".into(),
    }
}

fn returns_101_function() -> FuncBody {
    let ret = word(0);
    let mut body = function(
        "main_returns_101",
        4,
        RetAbi::Scalar(ret),
        Terminator::Return,
    );
    body.blocks[0].stmts.push(Stmt::Assign {
        dst: ScalarPlace::Slot(ret),
        rv: Rvalue::Use(Operand::Imm {
            bits: 101,
            width: Width::W64,
        }),
    });
    body
}

fn main_module(body: FuncBody) -> Module {
    let mut module = Module {
        funcs: vec![body].into(),
        entry: Some(EntryPlan {
            lang_start: 0,
            main_addr: LinkAddr(0),
            argc: 0,
            argv_ptr: 0,
            sigpipe: 0,
        }),
        ..Module::default()
    };
    install_fake_guest_panic_cleanup(&mut module);
    module
}

fn p1_identity_module(link_addr: LinkAddr, unwind: bool) -> Module {
    let ret = word(0);
    let mut body = function("p1_identity", 0, RetAbi::Scalar(ret), Terminator::Return);
    body.blocks[0].stmts.push(Stmt::Assign {
        dst: ScalarPlace::Slot(ret),
        rv: Rvalue::Use(Operand::Imm {
            bits: 73,
            width: Width::W64,
        }),
    });
    let sig = ForeignSig {
        args: Vec::new(),
        ret: FfiKind::U64,
        fixed: None,
        thunk_args: Vec::new(),
        unwind,
    };
    let mut module = Module {
        funcs: vec![body].into(),
        ..Module::default()
    };
    module.fn_entry_links.push((link_addr, 0));
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig,
    });
    module
}

fn lifecycle_callback_library(link_addr: LinkAddr, attribute: &str) -> (NativeFixtureDir, PathBuf) {
    assert!(matches!(attribute, "constructor" | "destructor"));
    let directory = NativeFixtureDir::new(&format!("{attribute}-engine-fault"));
    let lifecycle = directory.path().join("lifecycle.c");
    let bridge = directory.path().join("bridge.S");
    let library = directory.path().join("lifecycle.so");
    std::fs::write(
        &lifecycle,
        format!(
            r#"
extern void callback(void);
__attribute__(({attribute})) static void lifecycle(void) {{ callback(); }}
"#
        ),
    )
    .unwrap();
    let slot = super::ir::native_entry_slot_name(link_addr);
    std::fs::write(
        &bridge,
        format!(
            r#"
.intel_syntax noprefix
.text
.globl callback
.hidden callback
.type callback,@function
callback:
    jmp QWORD PTR [rip + {slot}]
.size callback,.-callback
.pushsection .data.mirvm_p1,"aw",@progbits
.p2align 3
.globl {slot}
.hidden {slot}
.type {slot},@object
.size {slot},8
{slot}:
    .quad 0
.popsection
.section .note.GNU-stack,"",@progbits
"#
        ),
    )
    .unwrap();
    let output = Command::new("cc")
        .args(["-shared", "-fPIC", "-Wl,-z,defs", "-Wl,-Bsymbolic"])
        .arg(&lifecycle)
        .arg(&bridge)
        .arg("-o")
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to build {attribute} fixture:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (directory, library)
}

fn constructor_signal_fault_archive(link_addr: LinkAddr) -> (NativeFixtureDir, PathBuf) {
    let directory = NativeFixtureDir::new("constructor-signal-fault-archive");
    let lifecycle = directory.path().join("lifecycle.c");
    let bridge = directory.path().join("bridge.S");
    let lifecycle_object = directory.path().join("lifecycle.o");
    let bridge_object = directory.path().join("bridge.o");
    let archive = directory.path().join("libconstructor_signal_fault.a");
    std::fs::write(
        &lifecycle,
        r#"
extern void callback(void);
__attribute__((constructor)) static void lifecycle(void) { callback(); }
"#,
    )
    .unwrap();
    let slot = super::ir::native_entry_slot_name(link_addr);
    std::fs::write(
        &bridge,
        format!(
            r#"
.intel_syntax noprefix
.text
.globl callback
.hidden callback
.type callback,@function
callback:
    jmp QWORD PTR [rip + {slot}]
.size callback,.-callback
.pushsection .data.mirvm_p1,"aw",@progbits
.p2align 3
.globl {slot}
.hidden {slot}
.type {slot},@object
.size {slot},8
{slot}:
    .quad 0
.popsection
.section .note.GNU-stack,"",@progbits
"#
        ),
    )
    .unwrap();
    for (source, object) in [(&lifecycle, &lifecycle_object), (&bridge, &bridge_object)] {
        let output = Command::new("cc")
            .args(["-fPIC", "-c"])
            .arg(source)
            .arg("-o")
            .arg(object)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to compile constructor signal-fault fixture:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = Command::new("ar")
        .arg("crs")
        .arg(&archive)
        .arg(&lifecycle_object)
        .arg(&bridge_object)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to archive constructor signal-fault fixture:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let library =
        crate::native::archive::materialize_in(&archive, &directory.path().join("materialized"))
            .unwrap();
    (directory, library)
}

fn constructor_faulting_masked_reraise_module(
    constructor: LinkAddr,
    handler: LinkAddr,
    library: &Path,
) -> Module {
    let count = word(0);
    let signal_handler = FuncBody {
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
                        bits: crate::os::signal::SIGUSR1 as u64,
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
                    "constructor signal handler trapped after its masked same-number raise".into(),
                ),
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
        ],
        name: "constructor_faulting_masked_signal_handler".into(),
    };
    let install_and_raise = FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
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
                        Operand::AddrImm(handler),
                    ],
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Continue,
                    role: super::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::CallBuiltin {
                    builtin: Builtin::HostRaise,
                    args: vec![Operand::Imm {
                        bits: crate::os::signal::SIGUSR1 as u64,
                        width: Width::W32,
                    }],
                    ret: RetDest::Ignore,
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
        name: "constructor_installs_and_raises_signal_handler".into(),
    };
    let mut module = Module {
        funcs: vec![signal_handler, install_and_raise].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.fn_entry_links.push((handler, 0));
    module.fn_entry_links.push((constructor, 1));
    module.entry_stub_sites.extend([
        super::ir::EntryStubSite {
            link_addr: handler,
            func: 0,
            sig: signal_callback_sig(),
        },
        super::ir::EntryStubSite {
            link_addr: constructor,
            func: 1,
            sig: callback_sig(),
        },
    ]);
    module
}

fn owner_fault_roundtrip_module() -> Module {
    let mut module = Module {
        funcs: vec![
            calls_native("owner_calls_nested_engine", nested_engine_entry),
            engine_fault_function("owner_engine_fault", 0),
            function(
                "owner_recovers_after_fault",
                0,
                RetAbi::Zst,
                Terminator::Return,
            ),
        ]
        .into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.exports.insert("recover".into(), 2);
    module
}

fn foreign_fault_traversal_module(owner_fault_thunk: u64) -> Module {
    let result = word(0);
    let try_addr = 0xc001;
    let catch_addr = 0xc002;
    let outer = FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Scalar(result),
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
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
                    unwind: UnwindAction::Cleanup(2),
                    role: crate::vm::ir::BuiltinCallRole::Normal,
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
            Block {
                stmts: vec![marker_store(&FOREIGN_FAULT_CLEANUP_RAN)],
                term: Terminator::Resume,
            },
        ],
        name: "foreign_fault_catch_frame".into(),
    };
    let try_body = FuncBody {
        frame_size: 16,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: vec![ParamAbi::Scalar(word(8))],
        caller_loc_off: None,
        blocks: vec![
            Block {
                stmts: Vec::new(),
                term: Terminator::CallIndirect {
                    callee: Operand::Imm {
                        bits: owner_fault_thunk,
                        width: Width::W64,
                    },
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 1,
                    unwind: UnwindAction::Cleanup(2),
                    null_ok: false,
                    native_sig: Some(callback_sig()),
                },
            },
            Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            },
            Block {
                stmts: vec![marker_store(&FOREIGN_FAULT_CLEANUP_RAN)],
                term: Terminator::Resume,
            },
        ],
        name: "foreign_fault_try_frame".into(),
    };
    let mut catch_body = function(
        "foreign_fault_must_not_reach_guest_catch",
        2,
        RetAbi::Zst,
        Terminator::Return,
    );
    catch_body.blocks[0]
        .stmts
        .push(marker_store(&FOREIGN_FAULT_CATCH_RAN));

    let mut module = Module {
        funcs: vec![outer, try_body, catch_body].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_entry_links.push((LinkAddr(try_addr), 1));
    module.fn_entry_links.push((LinkAddr(catch_addr), 2));
    module
}

fn foreign_closed_traversal_module(owner_thunk: u64) -> Module {
    let mut module = Module {
        funcs: vec![FuncBody {
            frame_size: 8,
            frame_align: 8,
            ret: RetAbi::Zst,
            params: Vec::new(),
            caller_loc_off: None,
            blocks: vec![
                Block {
                    stmts: Vec::new(),
                    term: Terminator::CallIndirect {
                        callee: Operand::Imm {
                            bits: owner_thunk,
                            width: Width::W64,
                        },
                        args: Vec::new(),
                        ret: RetDest::Ignore,
                        target: 1,
                        unwind: UnwindAction::Cleanup(2),
                        null_ok: false,
                        native_sig: Some(callback_sig()),
                    },
                },
                Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                },
                Block {
                    stmts: vec![marker_store(&FOREIGN_CLOSED_CLEANUP_RAN)],
                    term: Terminator::Resume,
                },
            ],
            name: "foreign_engine_closed_cleanup_frame".into(),
        }]
        .into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module
}

fn caller_catches_owner_panic(thunk: u64) -> Module {
    let result = word(0);
    let try_addr = 0xb001;
    let catch_addr = 0xb002;
    let outer = function(
        "caller_catch_unwind",
        0,
        RetAbi::Scalar(result),
        Terminator::CallBuiltin {
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
            role: crate::vm::ir::BuiltinCallRole::Normal,
        },
    );
    let try_body = function(
        "caller_try_invokes_owner_thunk",
        1,
        RetAbi::Zst,
        Terminator::CallIndirect {
            callee: Operand::Imm {
                bits: thunk,
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
    let mut catch_body = function(
        "caller_must_not_catch_owner_exception",
        2,
        RetAbi::Zst,
        Terminator::Return,
    );
    catch_body.blocks[0].stmts.push(Stmt::AtomicStore {
        addr: Operand::Imm {
            bits: FOREIGN_CATCH_RAN.as_ptr() as u64,
            width: Width::W64,
        },
        val: Operand::Imm {
            bits: 1,
            width: Width::W64,
        },
        order: MemOrd::SeqCst,
    });

    let mut module = Module {
        funcs: vec![outer, try_body, catch_body].into(),
        ..Module::default()
    };
    module.exports.insert("probe".into(), 0);
    module.fn_entry_links.push((LinkAddr(try_addr), 1));
    module.fn_entry_links.push((LinkAddr(catch_addr), 2));
    module
}

#[test]
fn guest_catch_does_not_consume_another_engines_panic() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        FOREIGN_CATCH_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            Module {
                funcs: vec![guest_panic_function("owner_guest_panic", 0)].into(),
                ..Module::default()
            },
            jit,
        );
        let thunk = thunks::get_or_create(owner.shared(), 0xa001, 0, &callback_sig());
        let caller = engine(caller_catches_owner_panic(thunk), jit);

        let exception = unwind::catch_raw(|| unsafe { run_export(&caller, "probe", &[]) })
            .expect_err("the owner Engine's guest panic must leave the caller Engine");
        let payload = match exception.take_mirvm(owner.shared()) {
            Ok(payload) => payload,
            Err(exception) => exception.resume_or_rethrow(),
        };
        let unwind::MirvmPayload::Guest(payload) = payload else {
            panic!("owner guest panic changed kind")
        };
        assert_eq!(payload.transfer(|_, inner| inner), 0x4747);
        if FOREIGN_CATCH_RAN.load(Ordering::SeqCst) != 0 {
            failures.push(mode);
        }
    }
    assert!(
        failures.is_empty(),
        "guest catch consumed a GuestPanic owned by another Engine in {failures:?}"
    );
}

#[test]
fn run_main_distinguishes_guest_panic_from_normal_exit_101() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let panicking = engine(main_module(guest_panic_function("main_panics", 4)), jit);
        let returning = engine(main_module(returns_101_function()), jit);
        let panic_result = run_main(&panicking);
        let return_result = run_main(&returning);

        if !matches!(panic_result, Ok(RunOutcome::GuestPanic))
            || !matches!(return_result, Ok(RunOutcome::Returned(101)))
        {
            failures.push((mode, panic_result, return_result));
        }
    }
    assert!(
        failures.is_empty(),
        "run_main collapsed guest panic and normal exit 101: {failures:?}"
    );
}

#[test]
fn run_result_has_structured_guest_panic_and_engine_fault_categories() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let guest = engine(export_module(guest_panic_function("export_panics", 0)), jit);
        let fault = engine(
            export_module(engine_fault_function("export_engine_fault", 0)),
            jit,
        );
        let guest_outcome = unsafe { run_export(&guest, "probe", &[]) }
            .expect("guest panic is a guest execution outcome");
        let fault_error = unsafe { run_export(&fault, "probe", &[]) }
            .expect_err("EngineFault must not be a successful export result");

        if guest_outcome != RunOutcome::GuestPanic || fault_error.kind != RunErrorKind::EngineFault
        {
            failures.push((mode, guest_outcome, fault_error));
        }
    }
    assert!(
        failures.is_empty(),
        "RunError has no structured guest-panic/engine-fault category: {failures:?}"
    );
}

#[test]
fn engine_fault_crosses_foreign_engine_and_is_finished_by_its_owner() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        FOREIGN_FAULT_CATCH_RAN.store(0, Ordering::SeqCst);
        FOREIGN_FAULT_CLEANUP_RAN.store(0, Ordering::SeqCst);
        FOREIGN_FAULT_BOUNDARY_RESULT.store(0, Ordering::SeqCst);

        let owner = engine(owner_fault_roundtrip_module(), jit);
        let owner_fault_thunk = thunks::get_or_create(owner.shared(), 0xd001, 1, &callback_sig());
        let foreign = engine(foreign_fault_traversal_module(owner_fault_thunk), jit);

        let fault_result =
            with_nested_engine(&foreign, || unsafe { run_export(&owner, "probe", &[]) });
        let boundary_result = FOREIGN_FAULT_BOUNDARY_RESULT.load(Ordering::SeqCst);
        let catch_ran = FOREIGN_FAULT_CATCH_RAN.load(Ordering::SeqCst);
        let cleanup_ran = FOREIGN_FAULT_CLEANUP_RAN.load(Ordering::SeqCst);
        let fault_still_in_flight = super::ctx::engine_fault_in_flight();
        let recovery = unsafe { run_export(&owner, "recover", &[]) };

        let owner_classified = matches!(
            &fault_result,
            Err(error)
                if error.kind == RunErrorKind::EngineFault
                    && error.exit_code == 70
                    && error.message.contains("embedding contract engine fault")
        );
        let owner_recovered = matches!(recovery, Ok(RunOutcome::Returned(value)) if value.lo == 0);
        if !owner_classified
            || boundary_result != 0
            || catch_ran != 0
            || cleanup_ran != 0
            || fault_still_in_flight
            || !owner_recovered
        {
            failures.push(format!(
                "{mode}: fault={fault_result:?}, B-boundary={boundary_result}, \
                 B-catch={catch_ran}, B-cleanup={cleanup_ran}, \
                 in-flight={fault_still_in_flight}, recovery={recovery:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "EngineFault changed owner or ran guest cleanup while crossing another Engine: {failures:#?}"
    );
}

#[test]
fn suspended_engine_fault_does_not_hide_reentrant_guest_panic_cleanup() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        SUSPENDED_FAULT_GUEST_CLEANUP_RAN.store(0, Ordering::SeqCst);

        let fault_owner = engine(Module::default(), false);
        let activation = super::ctx::activate(fault_owner.shared());
        let suspended = unwind::catch_raw(|| {
            unwind::raise_engine_fault(activation.ctx(), "suspended by a native catch".into(), 70)
        })
        .expect_err("EngineFault must reach the native catch");

        let reentrant = engine(export_module(guest_panic_with_cleanup_function()), jit);
        let outcome = unsafe { run_export(&reentrant, "probe", &[]) };
        let cleanup_ran = SUSPENDED_FAULT_GUEST_CLEANUP_RAN.load(Ordering::SeqCst);

        let payload = suspended
            .take_mirvm(fault_owner.shared())
            .expect("the suspended EngineFault must retain its owner");
        let unwind::MirvmPayload::EngineFault(fault) = payload else {
            panic!("suspended EngineFault changed kind")
        };
        let report = fault.finish();

        if !matches!(outcome, Ok(RunOutcome::GuestPanic))
            || cleanup_ran != 1
            || report.message != "suspended by a native catch"
            || super::ctx::engine_fault_in_flight()
        {
            failures.push(format!(
                "{mode}: nested={outcome:?}, cleanup={cleanup_ran}, report={report:?}, in-flight={}",
                super::ctx::engine_fault_in_flight()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "a suspended EngineFault hid cleanup for a distinct reentrant guest panic: {failures:#?}"
    );
}

#[test]
fn suspended_engine_fault_allows_a_reentrant_engine_fault() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let outer_owner = engine(Module::default(), false);
        let activation = super::ctx::activate(outer_owner.shared());
        let suspended = unwind::catch_raw(|| {
            unwind::raise_engine_fault(activation.ctx(), "outer suspended EngineFault".into(), 70)
        })
        .expect_err("outer EngineFault must reach the native catch");

        let reentrant = engine(
            export_module(engine_fault_function("reentrant_engine_fault", 0)),
            jit,
        );
        let inner = unsafe { run_export(&reentrant, "probe", &[]) };
        let outer_remained_live = super::ctx::engine_fault_in_flight();

        let payload = suspended
            .take_mirvm(outer_owner.shared())
            .expect("the outer EngineFault must retain its owner");
        let unwind::MirvmPayload::EngineFault(fault) = payload else {
            panic!("outer EngineFault changed kind")
        };
        let report = fault.finish();

        if !matches!(
            inner,
            Err(ref error)
                if error.kind == RunErrorKind::EngineFault
                    && error.message.contains("embedding contract engine fault")
        ) || !outer_remained_live
            || report.message != "outer suspended EngineFault"
            || super::ctx::engine_fault_in_flight()
        {
            failures.push(format!(
                "{mode}: inner={inner:?}, outer-live={outer_remained_live}, report={report:?}, in-flight={}",
                super::ctx::engine_fault_in_flight()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "a suspended EngineFault prevented an independent reentrant EngineFault: {failures:#?}"
    );
}

#[test]
fn engine_top_rethrows_host_rust_panic_unchanged() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let host = engine(
            export_module(calls_native("host_panic_export", host_panic_entry)),
            jit,
        );
        let caught = catch_unwind(AssertUnwindSafe(|| unsafe {
            run_export(&host, "probe", &[])
        }));
        match caught {
            Err(payload)
                if payload.downcast_ref::<HostPanicMarker>() == Some(&HostPanicMarker(0xe13)) => {}
            Err(_) => failures.push(format!("{mode}: host panic payload type or value changed")),
            Ok(result) => failures.push(format!(
                "{mode}: Engine boundary consumed host panic as {result:?}"
            )),
        }
        if super::ctx::engine_fault_in_flight() {
            failures.push(format!(
                "{mode}: host panic incorrectly left EngineFault state active"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Engine top did not preserve a host Rust panic: {failures:#?}"
    );
}

#[test]
fn interpreter_prologue_fault_restores_frame_state() {
    let broken = engine(
        export_module(function(
            "missing_required_argument",
            1,
            RetAbi::Zst,
            Terminator::Return,
        )),
        false,
    );
    let activation = super::ctx::activate(broken.shared());
    let ctx = activation.ctx();
    assert_eq!(unsafe { (*ctx).depth }, 0);
    assert_eq!(unsafe { (*ctx).region.used() }, 0);

    let error = unsafe { run_export(&broken, "probe", &[]) }
        .expect_err("missing argument must be an EngineFault");
    assert_eq!(error.kind, RunErrorKind::EngineFault);
    assert_eq!(unsafe { (*ctx).depth }, 0);
    assert_eq!(unsafe { (*ctx).region.used() }, 0);
    assert!(unsafe { (*ctx).shadow.is_empty() });

    let outcome = unsafe { run_export(&broken, "probe", &[7]) }.unwrap();
    assert_eq!(outcome, RunOutcome::Returned(Default::default()));
    assert_eq!(unsafe { (*ctx).depth }, 0);
    assert_eq!(unsafe { (*ctx).region.used() }, 0);
    assert!(unsafe { (*ctx).shadow.is_empty() });
}

#[test]
fn close_waits_for_a_real_active_call_before_teardown() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        reset_lifecycle_gate();
        let engine = engine(
            export_module(calls_native(
                "lifecycle_blocking_export",
                lifecycle_blocking_entry,
            )),
            jit,
        );
        let id = engine.shared().id;
        let execution = engine.clone();
        let running = std::thread::spawn(move || unsafe { run_export(&execution, "probe", &[]) });
        wait_lifecycle_entry();

        engine.close();
        let state_while_active = engine.state();
        let registry_while_active = super::ctx::engine(id).is_some();
        let rejected = unsafe { run_export(&engine, "probe", &[]) };
        release_lifecycle_entry();
        let completed = running.join().expect("active guest call thread panicked");
        engine.wait_closed().unwrap();

        if state_while_active != super::ctx::EngineState::Closing
            || !registry_while_active
            || !matches!(
                rejected,
                Err(ref error) if error.kind == RunErrorKind::EngineClosed
            )
            || !matches!(completed, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || engine.state() != super::ctx::EngineState::Closed
            || super::ctx::engine(id).is_some()
        {
            failures.push(format!(
                "{mode}: active-state={state_while_active:?}, registry={registry_while_active}, \
                 rejected={rejected:?}, completed={completed:?}, final={:?}",
                engine.state()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Engine close tore down active execution or admitted new work: {failures:#?}"
    );
}

#[test]
fn suspended_guest_exception_keeps_engine_closing_until_consumed() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        let owner = engine(
            Module {
                funcs: vec![guest_panic_function("suspended_guest_panic", 0)].into(),
                ..Module::default()
            },
            jit,
        );
        let code = thunks::get_or_create(owner.shared(), 0xe225, 0, &callback_sig());
        let callback: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(code as usize) };
        let exception = unwind::catch_raw(|| unsafe { callback() })
            .expect_err("guest panic must be suspended outside the callback thunk");

        owner.close();
        let state_while_suspended = owner.state();

        let payload = exception
            .take_mirvm(owner.shared())
            .expect("suspended guest panic changed owner");
        let unwind::MirvmPayload::Guest(payload) = payload else {
            panic!("suspended guest panic changed kind")
        };
        let inner = payload.transfer(|_, inner| inner);
        owner.wait_closed().unwrap();

        if state_while_suspended != super::ctx::EngineState::Closing
            || owner.state() != super::ctx::EngineState::Closed
            || inner != 0x4747
        {
            failures.push(format!(
                "{mode}: suspended={state_while_suspended:?}, final={:?}, inner={inner:#x}",
                owner.state()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "Engine finalized while a native catch retained its guest exception: {failures:#?}"
    );
}

#[test]
fn wait_closed_from_own_native_callback_fails_fast() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        LIFECYCLE_WAIT_RESULT.store(0, Ordering::SeqCst);
        let engine = engine(
            export_module(calls_native(
                "wait_closed_from_native_callback",
                lifecycle_wait_from_callback,
            )),
            jit,
        );

        let result = with_nested_engine(&engine, || unsafe { run_export(&engine, "probe", &[]) });
        let outer_wait = engine.wait_closed();
        if !matches!(result, Ok(RunOutcome::Returned(value)) if value.lo == 0)
            || LIFECYCLE_WAIT_RESULT.load(Ordering::SeqCst) != 1
            || outer_wait.is_err()
            || engine.state() != super::ctx::EngineState::Closed
        {
            failures.push(format!(
                "{mode}: result={result:?}, callback-wait={}, outer-wait={outer_wait:?}, state={:?}",
                LIFECYCLE_WAIT_RESULT.load(Ordering::SeqCst),
                engine.state()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "wait_closed blocked on its own callback lease: {failures:#?}"
    );
}

#[test]
fn same_engine_thunk_reentry_cannot_claim_outer_main_catcher() {
    THUNK_MAIN_CATCH_RESULT.store(0, Ordering::SeqCst);
    let engine = engine(
        Module {
            funcs: vec![calls_native(
                "thunk_attempts_outer_main_catch",
                thunk_attempt_main_catch,
            )]
            .into(),
            ..Module::default()
        },
        false,
    );
    let code = thunks::get_or_create(engine.shared(), 0xe222, 0, &callback_sig());
    let callback: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(code as usize) };

    let activation = super::ctx::activate(engine.shared());
    let ctx = activation.ctx();
    let run = super::ctx::begin_main_run(ctx);
    super::ctx::call_main_panic_boundary(ctx, || {
        unsafe { callback() };
        assert_eq!(
            THUNK_MAIN_CATCH_RESULT.load(Ordering::SeqCst),
            1,
            "same-Engine thunk reentry claimed the outer main catcher"
        );
        let outer =
            super::ctx::claim_main_panic_catch(ctx, super::ir::BuiltinCallRole::MainPanicCatcher);
        assert!(
            outer.is_some(),
            "outer activation could no longer claim its catcher"
        );
    });
    assert!(!run.finish());
    drop(activation);
    engine.wait_closed().unwrap();
}

#[test]
fn closed_c_unwind_thunk_raises_structured_engine_closed() {
    let engine = engine(
        Module {
            funcs: vec![function(
                "closed_thunk_target",
                0,
                RetAbi::Zst,
                Terminator::Return,
            )]
            .into(),
            ..Module::default()
        },
        false,
    );
    let control = std::sync::Arc::clone(engine.control());
    let code = thunks::get_or_create(engine.shared(), 0xe220, 0, &callback_sig());
    engine.wait_closed().unwrap();

    let callback: unsafe extern "C-unwind" fn() = unsafe { std::mem::transmute(code as usize) };
    let exception = unwind::catch_raw(|| unsafe { callback() })
        .expect_err("closed C-unwind thunk must raise EngineClosed");
    exception
        .take_engine_closed(&control)
        .expect("closed thunk raised the wrong exception kind or owner");
}

#[test]
fn p1_entries_are_per_engine_and_never_aba_after_close() {
    let link_addr = LinkAddr(0x6b00_0000_0100);
    let first = engine(p1_identity_module(link_addr, true), false);
    let second = engine(p1_identity_module(link_addr, true), false);
    let first_control = std::sync::Arc::clone(first.control());
    let first_addr = first.shared().instance.resolve_link_addr(link_addr);
    let second_addr = second.shared().instance.resolve_link_addr(link_addr);
    assert_ne!(
        first_addr, second_addr,
        "each Engine must own a distinct P1 closure"
    );

    let first_call: unsafe extern "C-unwind" fn() -> u64 =
        unsafe { std::mem::transmute(first_addr as usize) };
    let second_call: unsafe extern "C-unwind" fn() -> u64 =
        unsafe { std::mem::transmute(second_addr as usize) };
    assert_eq!(unsafe { first_call() }, 73);
    assert_eq!(unsafe { second_call() }, 73);

    first.wait_closed().unwrap();
    unwind::catch_raw(|| unsafe { first_call() })
        .expect_err("old P1 pointer must report its closed owner")
        .take_engine_closed(&first_control)
        .expect("old P1 pointer changed owner after close");
    assert_eq!(unsafe { second_call() }, 73, "closing A must not affect B");

    let third = engine(p1_identity_module(link_addr, true), false);
    let third_addr = third.shared().instance.resolve_link_addr(link_addr);
    assert_ne!(
        third_addr, first_addr,
        "P1 closure addresses must never be reused"
    );
    assert_ne!(third_addr, second_addr);
    unwind::catch_raw(|| unsafe { first_call() })
        .expect_err("old P1 pointer must stay a tombstone after a new Engine opens")
        .take_engine_closed(&first_control)
        .expect("old P1 pointer ABA-routed into the new Engine");
}

#[test]
fn constructor_guest_trap_returns_engine_initialization_error() {
    let link_addr = LinkAddr(0x6b00_0000_0300);
    let (_directory, library) = lifecycle_callback_library(link_addr, "constructor");
    let mut module = Module {
        funcs: vec![engine_fault_function("constructor_trap", 0)].into(),
        required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
        ..Module::default()
    };
    module.fn_entry_links.push((link_addr, 0));
    module.entry_stub_sites.push(super::ir::EntryStubSite {
        link_addr,
        func: 0,
        sig: callback_sig(),
    });

    let shared = Shared::new(module);
    let error = match Engine::try_new(shared) {
        Ok(_) => panic!("constructor guest Trap unexpectedly initialized an Engine"),
        Err(error) => error,
    };
    assert!(
        error.contains("native constructor hit an Engine fault")
            && error.contains("embedding contract engine fault"),
        "constructor Trap returned the wrong startup error: {error}"
    );
}

#[test]
fn constructor_signal_fault_drains_masked_reraise_before_failed_engine_closes() {
    const CHILD: &str = "MIRVM_CONSTRUCTOR_SIGNAL_FAULT_CHILD";
    if let Some(mode) = std::env::var_os(CHILD) {
        let _serial = SIGNAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_restore, baseline) =
            SavedSignalDisposition::replace_with_native(crate::os::signal::SIGUSR1);
        SIGNAL_OWNER_HANDLER_RAN.store(0, Ordering::SeqCst);
        // A host embedding thread may already block an unrelated signal. That
        // makes failed startup close on its worker rather than inline, so only
        // the constructor pthread can consume its target-thread SIGUSR1 cell.
        let unrelated_mask = crate::os::signal::Sigaction::for_signal(crate::os::signal::SIG_DFL);
        let _unrelated_mask = unrelated_mask
            .block_for_handler(crate::os::signal::SIGTERM)
            .unwrap();
        let constructor = LinkAddr(0x6b00_0000_0310);
        let handler = LinkAddr(0x6b00_0000_0320);
        let (_directory, library) = constructor_signal_fault_archive(constructor);
        let module = constructor_faulting_masked_reraise_module(constructor, handler, &library);
        let mut shared = Shared::new(module);
        shared.jit.enabled = mode == "jit";
        if shared.jit.enabled {
            shared.jit.threshold = 1;
            shared.jit.sync = true;
        }

        let error = match Engine::try_new(shared) {
            Ok(_) => panic!("faulting constructor signal handler initialized an Engine"),
            Err(error) => error,
        };
        let restored = crate::os::signal::Sigaction::query(crate::os::signal::SIGUSR1).unwrap();
        assert!(
            error.contains("native constructor hit an Engine fault")
                && error.contains("constructor signal handler trapped"),
            "constructor signal fault returned the wrong startup error: {error}"
        );
        assert_eq!(
            SIGNAL_OWNER_HANDLER_RAN.load(Ordering::SeqCst),
            2,
            "constructor EngineFault boundary did not drain the accepted same-number raise"
        );
        assert!(restored.same_disposition(&baseline));
        return;
    }

    let test_name = "vm::embed_tests::engine_lifecycle::constructor_signal_fault_drains_masked_reraise_before_failed_engine_closes";
    let mut failures = Vec::new();
    for (mode, _) in modes() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
            .env(CHILD, mode)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to start constructor signal-fault subprocess");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
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
        match status {
            Some(status) if status.success() => {}
            Some(status) => failures.push(format!(
                "{mode}: status={status}\n{stdout}{stderr}"
            )),
            None => failures.push(format!(
                "{mode}: constructor EngineFault left its target-thread signal pending and timed out\n{stdout}{stderr}"
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "constructor signal fault did not finish startup failure: {failures:#?}"
    );
}

#[test]
fn finalizer_engine_fault_aborts_inside_teardown_boundary() {
    const CHILD: &str = "MIRVM_FINALIZER_ENGINE_FAULT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let link_addr = LinkAddr(0x6b00_0000_0400);
        let (_directory, library) = lifecycle_callback_library(link_addr, "destructor");
        let mut module = Module {
            funcs: vec![engine_fault_function("finalizer_trap", 0)].into(),
            required_native_libs: vec![library.to_string_lossy().into_owned().into_boxed_str()],
            ..Module::default()
        };
        module.fn_entry_links.push((link_addr, 0));
        module.entry_stub_sites.push(super::ir::EntryStubSite {
            link_addr,
            func: 0,
            sig: callback_sig(),
        });

        let engine = Engine::try_new(Shared::new(module)).unwrap();
        engine.wait_closed().unwrap();
        panic!("native finalizer EngineFault escaped teardown");
    }

    let test_name =
        "vm::embed_tests::engine_lifecycle::finalizer_engine_fault_aborts_inside_teardown_boundary";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("failed to start native finalizer EngineFault subprocess");
    assert!(
        !output.status.success(),
        "native finalizer EngineFault returned from teardown"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("native finalizer unwound during Engine teardown"),
        "native finalizer failed for the wrong reason:\n{stderr}"
    );
}

#[test]
fn closed_plain_c_p1_entry_aborts() {
    const CHILD: &str = "MIRVM_CLOSED_P1_C_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let link_addr = LinkAddr(0x6b00_0000_0200);
        let engine = engine(p1_identity_module(link_addr, false), false);
        let addr = engine.shared().instance.resolve_link_addr(link_addr);
        engine.wait_closed().unwrap();
        let callback: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(addr as usize) };
        let _ = unsafe { callback() };
        panic!("closed plain C P1 entry returned");
    }

    let test_name = "vm::embed_tests::engine_lifecycle::closed_plain_c_p1_entry_aborts";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("failed to start closed plain C P1 subprocess");
    assert!(
        !output.status.success(),
        "closed plain C P1 returned normally"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("plain C thunk called after its Engine was closed"),
        "closed plain C P1 failed for the wrong reason:\n{stderr}"
    );
}

#[test]
fn engine_closed_runs_cleanup_while_crossing_another_engine() {
    let mut failures = Vec::new();
    for (mode, jit) in modes() {
        FOREIGN_CLOSED_CLEANUP_RAN.store(0, Ordering::SeqCst);
        let owner = engine(
            Module {
                funcs: vec![function(
                    "closed_cleanup_owner",
                    0,
                    RetAbi::Zst,
                    Terminator::Return,
                )]
                .into(),
                ..Module::default()
            },
            false,
        );
        let control = std::sync::Arc::clone(owner.control());
        let thunk = thunks::get_or_create(owner.shared(), 0xe223, 0, &callback_sig());
        owner.wait_closed().unwrap();

        let foreign = engine(foreign_closed_traversal_module(thunk), jit);
        let exception = unwind::catch_raw(|| unsafe { run_export(&foreign, "probe", &[]) })
            .expect_err("the closed owner exception must cross the foreign Engine");
        let correctly_owned = exception.take_engine_closed(&control).is_ok();
        let cleanup_ran = FOREIGN_CLOSED_CLEANUP_RAN.load(Ordering::SeqCst);
        if !correctly_owned || cleanup_ran != 1 {
            failures.push(format!(
                "{mode}: owner={correctly_owned}, foreign-cleanup={cleanup_ran}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "EngineClosed skipped another Engine's guest cleanup: {failures:#?}"
    );
}

#[test]
fn engine_closed_obeys_a_guest_terminate_boundary() {
    const CHILD: &str = "MIRVM_ENGINE_CLOSED_TERMINATE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let owner = engine(
            Module {
                funcs: vec![function(
                    "closed_terminate_owner",
                    0,
                    RetAbi::Zst,
                    Terminator::Return,
                )]
                .into(),
                ..Module::default()
            },
            false,
        );
        let thunk = thunks::get_or_create(owner.shared(), 0xe224, 0, &callback_sig());
        owner.wait_closed().unwrap();
        let callback: unsafe extern "C-unwind" fn() =
            unsafe { std::mem::transmute(thunk as usize) };
        unwind::guard_terminate(|| unsafe { callback() });
        panic!("EngineClosed escaped a Terminate boundary");
    }

    let test_name =
        "vm::embed_tests::engine_lifecycle::engine_closed_obeys_a_guest_terminate_boundary";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("failed to start EngineClosed Terminate subprocess");
    assert!(
        !output.status.success(),
        "Terminate child returned normally"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unwind reached Terminate boundary"),
        "Terminate child failed for the wrong reason:\n{stderr}"
    );
}
