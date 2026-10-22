use super::*;
use crate::vm::ctx::{Engine, activate};

fn slot(off: u32) -> Slot {
    Slot {
        off,
        width: Width::W64,
    }
}
fn imm(bits: u64) -> Operand {
    Operand::Imm {
        bits,
        width: Width::W64,
    }
}
fn place(off: u32, deref: bool) -> ir::PlaceExpr {
    ir::PlaceExpr {
        base: ir::PlaceBase::Local(off),
        steps: if deref {
            vec![ir::PlaceStep::Deref].into()
        } else {
            Vec::new().into()
        },
    }
}
fn assign(off: u32, rv: ir::Rvalue) -> Stmt {
    Stmt::Assign {
        dst: ScalarPlace::Slot(slot(off)),
        rv,
    }
}
fn add(a: Operand, b: Operand) -> ir::Rvalue {
    ir::Rvalue::IntBin {
        op: IntBinOp::Add,
        signed: false,
        a,
        b,
    }
}

/// An escaped byte is accessed through its original pointer on both sides of the transfer.
/// The pre-loop store is externally observable, and a dictionary operand changes on specialization.
fn loop_body(generic: bool) -> ir::FuncBody {
    let byte = Slot {
        off: 40,
        width: Width::W8,
    };
    let mut body = ir::FuncBody {
        shared: None,
        frame_size: 128,
        frame_align: 64,
        ret: RetAbi::Pair(slot(8), byte),
        params: vec![ParamAbi::Scalar(slot(16)), ParamAbi::Scalar(slot(24))],
        caller_loc_off: None,
        name: "osr_loop".into(),
        blocks: vec![
            ir::Block {
                stmts: vec![
                    Stmt::Assign {
                        dst: ScalarPlace::Mem {
                            expr: place(24, true),
                            width: Width::W64,
                        },
                        rv: add(
                            Operand::Mem {
                                expr: place(24, true),
                                width: Width::W64,
                            },
                            imm(1),
                        ),
                    },
                    assign(0, ir::Rvalue::Use(imm(0))),
                    assign(8, ir::Rvalue::Use(imm(0))),
                    assign(32, ir::Rvalue::Ref(place(40, false))),
                    Stmt::Assign {
                        dst: ScalarPlace::Slot(byte),
                        rv: ir::Rvalue::Use(imm(7)),
                    },
                ],
                term: Terminator::Goto(1),
            },
            ir::Block {
                stmts: vec![
                    assign(0, add(Operand::Slot(slot(0)), imm(1))),
                    assign(
                        8,
                        add(
                            Operand::Slot(slot(8)),
                            if generic {
                                Operand::Mem {
                                    expr: ir::PlaceExpr {
                                        base: ir::PlaceBase::Local(96),
                                        steps: vec![ir::PlaceStep::Deref, ir::PlaceStep::Offset(0)]
                                            .into(),
                                    },
                                    width: Width::W64,
                                }
                            } else {
                                imm(3)
                            },
                        ),
                    ),
                    Stmt::Assign {
                        dst: ScalarPlace::Mem {
                            expr: place(32, true),
                            width: Width::W8,
                        },
                        rv: add(Operand::Slot(byte), imm(1)),
                    },
                    assign(
                        72,
                        ir::Rvalue::IntCmp {
                            cc: IntCc::Lt,
                            signed: false,
                            a: Operand::Slot(slot(0)),
                            b: Operand::Slot(slot(16)),
                        },
                    ),
                ],
                term: Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(Operand::Slot(slot(72))),
                    targets: vec![(1, 1)],
                    otherwise: 2,
                },
            },
            ir::Block {
                stmts: vec![],
                term: Terminator::Return,
            },
        ],
    };
    if generic {
        body.shared = Some(Box::new(ir::SharedBody {
            template: std::sync::Arc::new(
                ir::GenericTemplate::new(ir::GenericInstructions {
                    dictionary: 96,
                    binding_count: 1,
                    blocks: std::mem::take(&mut body.blocks),
                })
                .unwrap(),
            ),
            values: vec![3],
            locals: vec![ir::FrameLocal {
                pointer: slot(104),
                off: 40,
                size: 1,
                align: 1,
            }],
            calls: Vec::new(),
        }));
    }
    body
}

fn engine(body: ir::FuncBody) -> Engine {
    let mut module = ir::Module {
        funcs: vec![body].into(),
        ..Default::default()
    };
    module.exports.insert("loop".into(), 0);
    let mut shared = Shared::new(module);
    shared.jit.enabled = true;
    shared.jit.osr_enabled = true;
    shared.jit.sync = false;
    shared.jit.threshold = u32::MAX;
    Engine::new(shared)
}

#[test]
fn osr_borrows_live_frame_and_specializes_without_replaying_entry() {
    for generic in [false, true] {
        let engine = engine(loop_body(generic));
        let mut compiler = Compiler::for_osr(engine.shared(), engine.shared().domain);
        compiler.compile_osr(0);
        let jit = &engine.shared().jit;
        let slots = jit.slots_for(engine.shared().domain);
        assert!(
            ![0, OSR_QUEUED, FAIL_SENTINEL].contains(&slots.osr[0].entry.load(Ordering::Acquire))
        );
        assert_eq!(slots.slots[0].load(Ordering::Acquire), 0);
        let range = jit
            .symbol_ranges()
            .into_iter()
            .find(|range| range.role == JitSymbolRole::OsrBody)
            .unwrap();
        assert_eq!(
            jit.guest_func_at(range.start),
            None,
            "the interpreter already owns the shadow frame"
        );
        let activation = activate(engine.shared());
        let ctx = activation.ctx();
        let mut effects = 0u64;
        let result =
            crate::vm::interp::interp_frame(ctx, 0, &[1027, &mut effects as *mut u64 as u64]);
        assert_eq!(result, (1027 * 3, (1027 + 7) & 255));
        assert_eq!(effects, 1);
        assert_eq!(jit.osr_transfers.load(Ordering::Relaxed), 1);
        assert_eq!(unsafe { (*ctx).depth }, 0);
        assert!(unsafe { (*ctx).shadow.is_empty() });
        // The continuation cannot be used when the extra boundary would exceed headroom.
        let saved = unsafe { (*ctx).stack_floor };
        let marker = 0u8;
        unsafe {
            (*ctx).stack_floor = &marker as *const u8 as usize - STACK_GUARD_MARGIN;
        }
        let result =
            crate::vm::interp::interp_frame(ctx, 0, &[1027, &mut effects as *mut u64 as u64]);
        unsafe {
            (*ctx).stack_floor = saved;
        }
        assert_eq!(result, (1027 * 3, (1027 + 7) & 255));
        assert_eq!(effects, 2);
        assert_eq!(jit.osr_transfers.load(Ordering::Relaxed), 1);
        std::mem::forget(compiler);
    }
}

#[test]
fn osr_engine_fault_restores_the_interpreter_owner() {
    let mut body = loop_body(false);
    body.blocks[2].term = Terminator::Trap("osr fault probe".into());
    let engine = engine(body);
    let mut compiler = Compiler::for_osr(engine.shared(), engine.shared().domain);
    compiler.compile_osr(0);
    let activation = activate(engine.shared());
    let ctx = activation.ctx();
    let mut effects = 0u64;
    for _ in 0..2 {
        let error = unsafe {
            crate::vm::interp::run_export(&engine, "loop", &[1027, &mut effects as *mut u64 as u64])
        }
        .unwrap_err();
        assert!(error.message.contains("osr fault probe"), "{error:?}");
        assert_eq!(unsafe { (*ctx).depth }, 0);
        assert!(unsafe { (*ctx).shadow.is_empty() });
    }
    assert_eq!(engine.shared().jit.osr_transfers.load(Ordering::Relaxed), 2);
    assert_eq!(effects, 2);
    std::mem::forget(compiler);
}

#[test]
fn osr_rejection_is_terminal_and_does_not_publish_normal_entries() {
    let mut body = loop_body(false);
    body.blocks.truncate(1);
    body.blocks[0].term = Terminator::Return;
    let engine = engine(body);
    let mut compiler = Compiler::for_osr(engine.shared(), engine.shared().domain);
    compiler.compile_osr(0);
    let slots = engine.shared().jit.slots_for(engine.shared().domain);
    assert_eq!(slots.osr[0].entry.load(Ordering::Acquire), FAIL_SENTINEL);
    assert_eq!(slots.slots[0].load(Ordering::Acquire), 0);
    assert_eq!(slots.slots_fast[0].load(Ordering::Acquire), 0);
}

#[test]
fn osr_partial_definition_cannot_be_entered() {
    let engine = engine(loop_body(false));
    let mut compiler = Compiler::for_osr(engine.shared(), engine.shared().domain);
    compiler.fail_after_symbol = Some(JitSymbolRole::OsrBody);
    compiler.compile_osr(0);
    let slots = engine.shared().jit.slots_for(engine.shared().domain);
    assert_eq!(slots.osr[0].entry.load(Ordering::Acquire), FAIL_SENTINEL);
    assert!(engine.shared().jit.symbol_ranges().is_empty());
}

#[test]
fn osr_demand_is_released_on_return_and_engine_fault() {
    for fault in [false, true] {
        let mut body = loop_body(false);
        if fault {
            body.blocks[2].term = Terminator::Trap("osr demand probe".into());
        }
        let engine = engine(body);
        let jit = &engine.shared().jit;
        jit.note_iterations(0, ITERATION_REQUEST);
        let (tx, rx) = std::sync::mpsc::channel();
        *jit.queue.lock().unwrap() = Some(tx);
        let activation = activate(engine.shared());
        let mut effects = 0u64;
        let result = unsafe {
            crate::vm::interp::run_export(&engine, "loop", &[1027, &mut effects as *mut u64 as u64])
        };
        assert_eq!(result.is_err(), fault);
        assert!(rx.try_recv().unwrap().osr);
        assert!(rx.try_recv().is_err());
        assert_eq!(jit.slots_for(engine.shared().domain).osr[0].waiting(), 0);
        assert!(!jit.begin_osr(engine.shared().domain, 0));
        assert_eq!(effects, 1);
        assert_eq!(unsafe { (*activation.ctx()).depth }, 0);
    }
}

#[test]
fn osr_worker_discards_stale_demand_before_constructing_code() {
    let engine = engine(loop_body(false));
    let jit = &engine.shared().jit;
    jit.note_iterations(0, ITERATION_REQUEST);
    let (tx, rx) = std::sync::mpsc::channel();
    *jit.queue.lock().unwrap() = Some(tx);
    let mut interest = None;
    jit.request_osr(engine.shared().domain, 0, &mut interest);
    drop(interest);
    jit.queue.lock().unwrap().take();
    super::super::worker(
        std::sync::Arc::clone(engine.shared()),
        rx,
        engine.shared().domain,
        Heat::default(),
    );
    assert_eq!(
        jit.slots_for(engine.shared().domain).osr[0]
            .entry
            .load(Ordering::Acquire),
        0
    );
    assert!(jit.symbol_ranges().is_empty());
}

#[test]
fn osr_trace_preserves_frame_and_unwind_boundaries() {
    const CHILD: &str = "MIRVM_OSR_TRACE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "vm::jit::compiler::osr::tests::osr_trace_preserves_frame_and_unwind_boundaries",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + crate::CHILD_HANG_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("OSR trace child timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let path = std::env::temp_dir().join(format!("mirvm-osr-{}.mlog", std::process::id()));
    let session = crate::telemetry::capture::CaptureSession::start(
        crate::telemetry::capture::StartOptions::new(
            &path,
            crate::telemetry::capture::STARTER_BYTES,
        ),
    )
    .unwrap();
    osr_borrows_live_frame_and_specializes_without_replaying_entry();
    osr_engine_fault_restores_the_interpreter_owner();
    osr_demand_is_released_on_return_and_engine_fault();
    drop(session);
    let _ = std::fs::remove_file(path);
}
