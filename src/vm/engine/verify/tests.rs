use super::*;

fn body(term: Terminator) -> FuncBody {
    FuncBody {
        frame_size: 8,
        frame_align: 8,
        ret: RetAbi::Zst,
        params: Vec::new(),
        caller_loc_off: None,
        blocks: vec![Block {
            stmts: Vec::new(),
            term,
        }],
        name: "test".into(),
    }
}

fn p1_sig() -> ForeignSig {
    ForeignSig {
        args: Vec::new(),
        ret: FfiKind::U64,
        fixed: None,
        thunk_args: Vec::new(),
        unwind: true,
    }
}

fn strict_p1_header(addr: LinkAddr) -> Module {
    let mut module = Module::default();
    module.funcs.push(body(Terminator::Return));
    module.link_fn_addrs.insert(addr, 0);
    module.entry_stub_sites.push(EntryStubSite {
        link_addr: addr,
        func: 0,
        sig: p1_sig(),
    });
    module.load_map.require_mapped();
    module
}

fn executable_with_roles(
    roles: &[(CallRole, UnwindAction)],
    catchers: &[(Builtin, UnwindAction)],
) -> Module {
    let mut module = Module::default();
    module.funcs.push(body(Terminator::Return));
    for &(role, unwind) in roles {
        module.funcs.push(body(Terminator::Call {
            callee: 0,
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 0,
            unwind,
            role,
        }));
    }
    for (builtin, unwind) in catchers {
        module.funcs.push(body(Terminator::CallBuiltin {
            builtin: builtin.clone(),
            args: vec![
                Operand::Imm {
                    bits: 0x1010,
                    width: Width::W64,
                },
                Operand::Imm {
                    bits: 0,
                    width: Width::W64,
                },
                Operand::Imm {
                    bits: 0x2020,
                    width: Width::W64,
                },
            ],
            ret: RetDest::Scalar(ScalarPlace::Slot(Slot {
                off: 0,
                width: Width::W8,
            })),
            target: 0,
            unwind: *unwind,
            role: BuiltinCallRole::MainPanicCatcher,
        }));
    }
    module.fn_addrs.insert(0x1010, 0);
    module.entry = Some(EntryPlan {
        lang_start: 0,
        main_addr: LinkAddr(0x1010),
        argc: 0,
        argv_ptr: 0,
        sigpipe: 0,
    });
    module.frozen = Some(super::super::frozen::FrozenArena::new());
    module
}

#[test]
fn executable_requires_exactly_one_main_panic_boundary() {
    let missing = executable_with_roles(&[], &[]);
    let err = module(&missing).unwrap_err();
    assert!(err.contains("0 main panic boundaries"), "{err}");

    let valid = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    module(&valid).unwrap();

    let duplicate = executable_with_roles(
        &[
            (CallRole::MainPanicBoundary, UnwindAction::Continue),
            (CallRole::MainPanicBoundary, UnwindAction::Continue),
        ],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    let err = module(&duplicate).unwrap_err();
    assert!(err.contains("2 main panic boundaries"), "{err}");
}

#[test]
fn main_panic_boundary_requires_continue_unwind() {
    let invalid = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Cleanup(0))],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    let err = module(&invalid).unwrap_err();
    assert!(err.contains("must use Continue"), "{err}");
}

#[test]
fn executable_requires_one_well_formed_main_panic_catcher() {
    let missing = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[],
    );
    let err = module(&missing).unwrap_err();
    assert!(err.contains("0 main panic catchers"), "{err}");

    let malformed = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[(Builtin::HostAbort, UnwindAction::Continue)],
    );
    let err = module(&malformed).unwrap_err();
    assert!(err.contains("must be CatchUnwind"), "{err}");

    let mut malformed_shape = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    let mut funcs = Vec::new();
    malformed_shape.funcs.drain_into(&mut funcs);
    let Terminator::CallBuiltin { args, ret, .. } = &mut funcs[2].blocks[0].term else {
        unreachable!()
    };
    args.pop();
    *ret = RetDest::Ignore;
    malformed_shape.funcs = funcs.into();
    let err = module(&malformed_shape).unwrap_err();
    assert!(err.contains("three pointer-width arguments"), "{err}");

    let mut malformed_width = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    let mut funcs = Vec::new();
    malformed_width.funcs.drain_into(&mut funcs);
    let Terminator::CallBuiltin { args, .. } = &mut funcs[2].blocks[0].term else {
        unreachable!()
    };
    args[1] = Operand::Imm {
        bits: 0,
        width: Width::W8,
    };
    malformed_width.funcs = funcs.into();
    let err = module(&malformed_width).unwrap_err();
    assert!(err.contains("three pointer-width arguments"), "{err}");
}

#[test]
fn partial_image_can_leave_main_boundary_in_an_earlier_layer() {
    let partial = executable_with_roles(&[], &[]);
    module_with_prefix(
        &partial,
        Prefix {
            funcs: 1,
            tls: 0,
            asm: 0,
        },
    )
    .unwrap();
}

#[test]
fn accepts_absolute_ids_in_a_partial_image() {
    let mut m = Module::default();
    m.funcs.push(body(Terminator::Call {
        callee: 1,
        args: Vec::new(),
        ret: RetDest::Ignore,
        target: 0,
        unwind: UnwindAction::Continue,
        role: crate::vm::engine::ir::CallRole::Normal,
    }));
    module_with_prefix(
        &m,
        Prefix {
            funcs: 1,
            tls: 0,
            asm: 0,
        },
    )
    .unwrap();
}

#[test]
fn rejects_bad_block_and_frame_slot() {
    let mut m = Module::default();
    let mut b = body(Terminator::Goto(1));
    b.params.push(ParamAbi::Scalar(Slot {
        off: 4,
        width: Width::W64,
    }));
    m.funcs.push(b);
    let err = module(&m).unwrap_err();
    assert!(err.contains("exceeds frame size"), "{err}");
}

#[test]
fn rejects_bad_external_references() {
    let mut m = Module::default();
    m.funcs.push(body(Terminator::Return));
    m.exports.insert("bad".into(), 1);
    let err = module(&m).unwrap_err();
    assert!(err.contains("function id 1"), "{err}");
}

#[test]
fn strict_artifact_requires_exactly_one_site_for_each_executable_entry() {
    let addr = LinkAddr(0x6100_0000_1000);
    let valid = strict_p1_header(addr);
    module(&valid).unwrap();

    let mut missing = strict_p1_header(addr);
    missing.entry_stub_sites.clear();
    let err = module(&missing).unwrap_err();
    assert!(err.contains("has no matching entry stub"), "{err}");

    let mut duplicate = strict_p1_header(addr);
    duplicate
        .entry_stub_sites
        .push(duplicate.entry_stub_sites[0].clone());
    let err = module(&duplicate).unwrap_err();
    assert!(err.contains("duplicates entry link address"), "{err}");
}

#[test]
fn entry_site_must_not_overlap_frozen_memory() {
    let mut frozen = super::super::frozen::FrozenArena::new();
    let addr = LinkAddr(frozen.alloc(8, 8));
    let mut artifact = strict_p1_header(addr);
    artifact.frozen = Some(frozen);
    artifact.rebuild_load_map();
    artifact.load_map.require_mapped();
    let err = module(&artifact).unwrap_err();
    assert!(err.contains("overlaps frozen memory"), "{err}");
}

#[test]
fn verifies_guest_panic_cleanup_function_ids() {
    let mut valid = Module::default();
    valid.funcs.push(body(Terminator::Return));
    valid.funcs.push(body(Terminator::Return));
    valid.guest_panic_cleanup = Some(GuestPanicCleanup {
        cleanup: 0,
        drop_payload: 1,
    });
    module(&valid).unwrap();

    let mut invalid = valid;
    invalid.guest_panic_cleanup = Some(GuestPanicCleanup {
        cleanup: 0,
        drop_payload: 2,
    });
    let err = module(&invalid).unwrap_err();
    assert!(err.contains("payload drop glue"), "{err}");
    assert!(err.contains("function id 2"), "{err}");
}

#[test]
fn accepts_guest_panic_cleanup_across_base_and_delta() {
    let mut delta = Module::default();
    delta.funcs.push(body(Terminator::Return));
    delta.guest_panic_cleanup = Some(GuestPanicCleanup {
        cleanup: 0,
        drop_payload: 2,
    });

    module_with_prefix(
        &delta,
        Prefix {
            funcs: 2,
            tls: 0,
            asm: 0,
        },
    )
    .unwrap();
}
