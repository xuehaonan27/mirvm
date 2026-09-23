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

/// A hand-built artifact plus the instance the verifier reads the loaded address state from. The
/// builders below seed both halves.
struct Artifact {
    module: Module,
    instance: Instance,
}

impl Artifact {
    fn new(module: Module) -> Self {
        let instance = Instance::materialize(&module).expect("test artifact instantiates");
        Artifact { module, instance }
    }

    fn verify(&self) -> Result<(), String> {
        super::module(&self.module, &self.instance)
    }

    fn verify_prefix(&self, prefix: Prefix) -> Result<(), String> {
        module_with_prefix(&self.module, &self.instance, prefix)
    }
}

fn strict_p1_header(addr: LinkAddr) -> Artifact {
    let mut module = Module::default();
    module.funcs.push(body(Terminator::Return));
    module.fn_entry_links.push((addr, 0));
    module.entry_stub_sites.push(EntryStubSite {
        link_addr: addr,
        func: 0,
        sig: p1_sig(),
    });
    let mut artifact = Artifact::new(module);
    artifact.instance.load_map.require_mapped();
    artifact
}

fn executable_with_roles(
    roles: &[(CallRole, UnwindAction)],
    catchers: &[(Builtin, UnwindAction)],
) -> Artifact {
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
    module.fn_entry_links.push((LinkAddr(0x1010), 0));
    module.entry = Some(EntryPlan {
        lang_start: 0,
        main_addr: LinkAddr(0x1010),
        argc: 0,
        argv_ptr: 0,
        sigpipe: 0,
    });
    let mut artifact = Artifact::new(module);
    artifact.instance.frozen = Some(super::super::frozen::FrozenArena::new());
    artifact.instance.rebuild_load_map();
    artifact
}

#[test]
fn executable_requires_exactly_one_main_panic_boundary() {
    let missing = executable_with_roles(&[], &[]);
    let err = missing.verify().unwrap_err();
    assert!(err.contains("0 main panic boundaries"), "{err}");

    let valid = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    valid.verify().unwrap();

    let duplicate = executable_with_roles(
        &[
            (CallRole::MainPanicBoundary, UnwindAction::Continue),
            (CallRole::MainPanicBoundary, UnwindAction::Continue),
        ],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    let err = duplicate.verify().unwrap_err();
    assert!(err.contains("2 main panic boundaries"), "{err}");
}

#[test]
fn main_panic_boundary_requires_continue_unwind() {
    let invalid = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Cleanup(0))],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    let err = invalid.verify().unwrap_err();
    assert!(err.contains("must use Continue"), "{err}");
}

#[test]
fn executable_requires_one_well_formed_main_panic_catcher() {
    let missing = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[],
    );
    let err = missing.verify().unwrap_err();
    assert!(err.contains("0 main panic catchers"), "{err}");

    let malformed = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[(Builtin::HostAbort, UnwindAction::Continue)],
    );
    let err = malformed.verify().unwrap_err();
    assert!(err.contains("must be CatchUnwind"), "{err}");

    let mut malformed_shape = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    let mut funcs = Vec::new();
    malformed_shape.module.funcs.drain_into(&mut funcs);
    let Terminator::CallBuiltin { args, ret, .. } = &mut funcs[2].blocks[0].term else {
        unreachable!()
    };
    args.pop();
    *ret = RetDest::Ignore;
    malformed_shape.module.funcs = funcs.into();
    let err = malformed_shape.verify().unwrap_err();
    assert!(err.contains("three pointer-width arguments"), "{err}");

    let mut malformed_width = executable_with_roles(
        &[(CallRole::MainPanicBoundary, UnwindAction::Continue)],
        &[(Builtin::CatchUnwind, UnwindAction::Continue)],
    );
    let mut funcs = Vec::new();
    malformed_width.module.funcs.drain_into(&mut funcs);
    let Terminator::CallBuiltin { args, .. } = &mut funcs[2].blocks[0].term else {
        unreachable!()
    };
    args[1] = Operand::Imm {
        bits: 0,
        width: Width::W8,
    };
    malformed_width.module.funcs = funcs.into();
    let err = malformed_width.verify().unwrap_err();
    assert!(err.contains("three pointer-width arguments"), "{err}");
}

#[test]
fn partial_image_can_leave_main_boundary_in_an_earlier_layer() {
    let partial = executable_with_roles(&[], &[]);
    partial
        .verify_prefix(Prefix {
            funcs: 1,
            tls: 0,
            asm: 0,
        })
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
        role: crate::vm::ir::CallRole::Normal,
    }));
    Artifact::new(m)
        .verify_prefix(Prefix {
            funcs: 1,
            tls: 0,
            asm: 0,
        })
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
    let err = Artifact::new(m).verify().unwrap_err();
    assert!(err.contains("exceeds frame size"), "{err}");
}

#[test]
fn rejects_bad_external_references() {
    let mut m = Module::default();
    m.funcs.push(body(Terminator::Return));
    m.exports.insert("bad".into(), 1);
    let err = Artifact::new(m).verify().unwrap_err();
    assert!(err.contains("function id 1"), "{err}");
}

#[test]
fn strict_artifact_requires_exactly_one_site_for_each_executable_entry() {
    let addr = LinkAddr(0x6100_0000_1000);
    let valid = strict_p1_header(addr);
    valid.verify().unwrap();

    let mut missing = strict_p1_header(addr);
    missing.module.entry_stub_sites.clear();
    let err = missing.verify().unwrap_err();
    assert!(err.contains("has no matching entry stub"), "{err}");

    let mut duplicate = strict_p1_header(addr);
    let first = duplicate.module.entry_stub_sites[0].clone();
    duplicate.module.entry_stub_sites.push(first);
    let err = duplicate.verify().unwrap_err();
    assert!(err.contains("duplicates entry link address"), "{err}");
}

#[test]
fn entry_site_must_not_overlap_frozen_memory() {
    let mut frozen = super::super::frozen::FrozenArena::new();
    let addr = LinkAddr(frozen.alloc(8, 8));
    let mut artifact = strict_p1_header(addr);
    artifact.instance.frozen = Some(frozen);
    artifact.instance.rebuild_load_map();
    artifact.instance.load_map.require_mapped();
    let err = artifact.verify().unwrap_err();
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
    Artifact::new(valid).verify().unwrap();

    let mut invalid = Module::default();
    invalid.funcs.push(body(Terminator::Return));
    invalid.funcs.push(body(Terminator::Return));
    invalid.guest_panic_cleanup = Some(GuestPanicCleanup {
        cleanup: 0,
        drop_payload: 2,
    });
    let err = Artifact::new(invalid).verify().unwrap_err();
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

    Artifact::new(delta)
        .verify_prefix(Prefix {
            funcs: 2,
            tls: 0,
            asm: 0,
        })
        .unwrap();
}
