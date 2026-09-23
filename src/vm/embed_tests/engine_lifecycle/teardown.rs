//! The teardown boundary: native constructors, finalizers and the close seal.

use super::*;

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
    let slot = super::super::ir::native_entry_slot_name(link_addr);
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
    let slot = super::super::ir::native_entry_slot_name(link_addr);
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
                    role: super::super::ir::BuiltinCallRole::Normal,
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
                    role: super::super::ir::BuiltinCallRole::Normal,
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
                    role: super::super::ir::BuiltinCallRole::Normal,
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
        super::super::ir::EntryStubSite {
            link_addr: handler,
            func: 0,
            sig: signal_callback_sig(),
        },
        super::super::ir::EntryStubSite {
            link_addr: constructor,
            func: 1,
            sig: callback_sig(),
        },
    ]);
    module
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
    module
        .entry_stub_sites
        .push(super::super::ir::EntryStubSite {
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

    let test_name = "vm::embed_tests::engine_lifecycle::teardown::constructor_signal_fault_drains_masked_reraise_before_failed_engine_closes";
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
        module
            .entry_stub_sites
            .push(super::super::ir::EntryStubSite {
                link_addr,
                func: 0,
                sig: callback_sig(),
            });

        let engine = Engine::try_new(Shared::new(module)).unwrap();
        engine.wait_closed().unwrap();
        panic!("native finalizer EngineFault escaped teardown");
    }

    let test_name = "vm::embed_tests::engine_lifecycle::teardown::finalizer_engine_fault_aborts_inside_teardown_boundary";
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
