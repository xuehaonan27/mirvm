use super::*;
use crate::vm::ir::*;

fn module() -> Module {
    let mut module = Module {
        funcs: (0..8)
            .map(|index| FuncBody {
                name: format!("body{index}").into(),
                frame_size: 8,
                frame_align: 8,
                ret: RetAbi::Scalar(Slot {
                    off: 0,
                    width: Width::W64,
                }),
                params: Vec::new(),
                caller_loc_off: None,
                blocks: vec![Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                }],
            })
            .collect(),
        ..Module::default()
    };
    module.ensure_function_names();
    module
}

fn load(module: &Module, instance: &Instance) -> Result<Image, Error> {
    let bytes = postcard::to_stdvec(&meta::ModuleMetaRef::from((module, instance))).unwrap();
    let metadata = postcard::from_bytes(&bytes).unwrap();
    Image::load(metadata, archive(module))
}

fn archive(module: &Module) -> functions::Functions {
    let bytes = bytes::Bytes::new(functions::encode(&module.funcs).unwrap().to_vec());
    let len = bytes.len();
    functions::Functions::load(bytes, 0..len).unwrap()
}

fn load_once(module: &Module, instance: &Instance) -> Result<VerifiedArtifact, Error> {
    let bytes = postcard::to_stdvec(&meta::ModuleMetaRef::from((module, instance))).unwrap();
    Image::load_once(postcard::from_bytes(&bytes).unwrap(), archive(module))
}

#[test]
fn engine_construction_preserves_lazy_bodies_and_fork_settles_them() {
    let image = load(&module(), &Instance::default()).unwrap();
    let artifact = image.instantiate().unwrap();
    assert_eq!(artifact.module().funcs.decoded_count(), 0);
    let engine = unsafe { crate::vm::Engine::from_verified_unchecked(artifact) }.unwrap();
    let funcs = &engine.shared().module.funcs;
    assert_eq!(funcs.decoded_count(), 0);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| assert_eq!(&*funcs[3].name, "body3"));
        }
    });
    assert_eq!(funcs.decoded_count(), 1);
    funcs.prepare_for_fork();
    assert_eq!(funcs.decoded_count(), funcs.len());
}

#[test]
fn merged_archives_verify_and_publish_without_decoding() {
    let mut base = module();
    // A backward reference in the suffix must retain its absolute id after merge and publication.
    let mut tail = module();
    let mut bodies = Vec::new();
    tail.funcs.drain_into(&mut bodies);
    bodies[0].blocks[0].term = Terminator::Call {
        callee: 3,
        args: Vec::new(),
        ret: RetDest::Ignore,
        target: 0,
        unwind: UnwindAction::Continue,
        role: CallRole::Normal,
    };
    tail.funcs = bodies.into();
    base.funcs = FuncTable::from_archive(Arc::new(archive(&base)));
    tail.funcs = FuncTable::from_archive(Arc::new(archive(&tail)));
    base.funcs.append(tail.funcs);
    base.function_names.extend(tail.function_names);
    base.funcs.append(vec![module().funcs[0].clone()].into());
    base.function_names.push("body0".into());
    assert_eq!(base.funcs.decoded_count(), 1);
    crate::vm::verify::module(&base, &Instance::default()).unwrap();
    let published = load(&base, &Instance::default()).unwrap();
    assert_eq!(base.funcs.decoded_count(), 1);
    let artifact = published.instantiate().unwrap();
    let funcs = &artifact.module().funcs;
    assert_eq!(funcs.len(), 17);
    assert_eq!(funcs.decoded_count(), 0);
    assert_eq!(&*funcs[7].name, "body7");
    assert!(matches!(
        funcs[8].blocks[0].term,
        Terminator::Call { callee: 3, .. }
    ));
    assert_eq!(&*funcs[15].name, "body7");
    assert_eq!(&*funcs[16].name, "body0");
    assert!(funcs.get(17).is_none());
    assert_eq!(funcs.decoded_count(), 4);
    funcs.prepare_for_fork();
    assert_eq!(funcs.decoded_count(), 17);

    // Removing the suffix invalidates this forward reference even though the bytes were archived.
    let mut bad = module();
    let mut bodies = Vec::new();
    bad.funcs.drain_into(&mut bodies);
    bodies[0].blocks[0].term = Terminator::Call {
        callee: 8,
        args: Vec::new(),
        ret: RetDest::Ignore,
        target: 0,
        unwind: UnwindAction::Continue,
        role: CallRole::Normal,
    };
    bad.funcs = bodies.into();
    bad.funcs = FuncTable::from_archive(Arc::new(archive(&bad)));
    assert!(crate::vm::verify::module(&bad, &Instance::default()).is_err());
    assert_eq!(bad.funcs.decoded_count(), 0);
}

#[test]
fn archive_and_owned_verifiers_reject_the_same_invalid_references() {
    let invalid = [
        Terminator::Goto(99),
        Terminator::SwitchInt {
            discr: SwitchDiscr::Scalar(Operand::Slot(Slot {
                off: 7,
                width: Width::W64,
            })),
            targets: Vec::new(),
            otherwise: 0,
        },
        Terminator::Call {
            callee: 99,
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 0,
            unwind: UnwindAction::Continue,
            role: CallRole::Normal,
        },
        Terminator::CallForeign {
            sym: "unused".into(),
            sig: ForeignSig {
                args: vec![FfiKind::Void],
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: Vec::new(),
                unwind: false,
            },
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 0,
            unwind: UnwindAction::Continue,
        },
    ];
    for term in invalid {
        let mut module = module();
        let mut bodies = Vec::new();
        module.funcs.drain_into(&mut bodies);
        bodies[0].blocks[0].term = term;
        module.funcs = bodies.into();
        let instance = Instance::default();
        let native = crate::vm::verify::module(&module, &instance).unwrap_err();
        let archived = load(&module, &instance)
            .err()
            .expect("invalid archive accepted");
        assert_eq!(native.to_string(), archived.to_string());
        let single = load_once(&module, &instance)
            .err()
            .expect("invalid archive accepted");
        assert_eq!(native.to_string(), single.to_string());
    }
}

#[test]
fn bound_image_restores_all_frozen_domains_independently() {
    use crate::os_arch::addrspace::{BASE_IMAGE_FIXED_ADDR, DELTA_FIXED_ADDR};
    use crate::vm::frozen::{FrozenArena, FrozenSnapshot};
    let mut module = module();
    module.frozen =
        Some(FrozenSnapshot::from_home_and_bytes(DELTA_FIXED_ADDR, vec![1; 8]).unwrap());
    let mut bodies = Vec::new();
    module.funcs.drain_into(&mut bodies);
    bodies[0].blocks[0].stmts.push(Stmt::Assign {
        dst: ScalarPlace::Slot(Slot {
            off: 0,
            width: Width::W64,
        }),
        rv: Rvalue::Use(Operand::Mem {
            expr: PlaceExpr {
                base: PlaceBase::Static(LinkAddr(BASE_IMAGE_FIXED_ADDR as u64)),
                steps: Box::new([]),
            },
            width: Width::W64,
        }),
    });
    module.funcs = bodies.into();
    let mut instance = Instance::materialize_dynamic(&module).unwrap();
    let base = FrozenSnapshot::from_home_and_bytes(BASE_IMAGE_FIXED_ADDR, vec![2; 8]).unwrap();
    instance
        .image_frozens
        .push(FrozenArena::restore_dynamic(&base).unwrap());
    instance.rebuild_load_map();
    let image = load(&module, &instance).unwrap();
    let (_, first) = image.instantiate().unwrap().into_parts();
    let (_, second) = image.instantiate().unwrap().into_parts();
    let single = load_once(&module, &instance).unwrap();
    assert_eq!(single.module().funcs.decoded_count(), 0);
    let (_, single) = single.into_parts();
    for address in [BASE_IMAGE_FIXED_ADDR, DELTA_FIXED_ADDR] {
        let a = first.resolve_link_addr(LinkAddr(address as u64));
        let b = second.resolve_link_addr(LinkAddr(address as u64));
        let c = single.resolve_link_addr(LinkAddr(address as u64));
        assert_ne!(a, c);
        assert_ne!(a, b);
        unsafe {
            assert_eq!(*(a as *const u8), *(b as *const u8));
            *(a as *mut u8) = 77;
            assert_ne!(*(a as *const u8), *(b as *const u8));
            assert_eq!(*(b as *const u8), *(c as *const u8));
        }
    }
}
