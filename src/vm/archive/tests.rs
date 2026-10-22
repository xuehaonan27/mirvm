use super::*;
use crate::vm::ir::*;

fn module() -> Module {
    let mut module = Module {
        funcs: (0..8)
            .map(|index| FuncBody {
                shared: None,
                name: format!("body{index}").into(),
                prepared: Default::default(),
                objects: Vec::new(),
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
fn generic_shared_archive_execution_and_concurrent_specialization() {
    let first = test_generic_body(11);
    let mut second = test_generic_body(29);
    second.shared.as_mut().unwrap().template = first.shared.as_ref().unwrap().template.clone();
    let mut module = Module {
        funcs: vec![first, second].into(),
        ..Module::default()
    };
    module.exports.insert("left".into(), 0);
    module.exports.insert("right".into(), 1);
    module.ensure_function_names();
    let source = archive(&module);
    let a = source.body(0).shared.as_ref().unwrap();
    let b = source.body(1).shared.as_ref().unwrap();
    assert!(std::ptr::eq(a.template.as_ref(), b.template.as_ref()));
    let image = load(&module, &Instance::default()).unwrap();
    let artifact = image.instantiate().unwrap();
    let independent = image.instantiate().unwrap();
    assert_eq!(artifact.module().funcs.decoded_count(), 0);
    assert_eq!(artifact.module().funcs.specialized_count(), 0);
    let engine = unsafe { crate::vm::Engine::from_verified_unchecked(artifact) }.unwrap();
    let funcs = &engine.shared().module.funcs;
    for (name, expected) in [("left", 18), ("right", 36)] {
        // SAFETY: both exports accept and return one u64.
        let result = unsafe { crate::vm::interp::run_export(&engine, name, &[7]) }.unwrap();
        assert_eq!(result.into_returned().unwrap().lo, expected);
    }
    assert_eq!(funcs.specialized_count(), 0);
    let first = funcs.execution(0).unwrap();
    let second = funcs.execution(1).unwrap();
    assert!(Arc::ptr_eq(
        &first.shared.as_ref().unwrap().template,
        &second.shared.as_ref().unwrap().template
    ));
    std::thread::scope(|scope| {
        let joins: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| funcs.get(0).unwrap() as *const _ as usize))
            .collect();
        let pointers: Vec<_> = joins.into_iter().map(|join| join.join().unwrap()).collect();
        assert!(pointers.iter().all(|pointer| *pointer == pointers[0]));
    });
    assert_eq!(funcs.specialized_count(), 1);
    assert!(first.shared.is_some());
    assert_eq!(funcs.get(0).unwrap().frame_size, first.frame_size);
    assert_eq!(
        postcard::to_stdvec(&funcs.get(0).unwrap().ret).unwrap(),
        postcard::to_stdvec(&first.ret).unwrap()
    );
    assert_eq!(independent.module().funcs.specialized_count(), 0);
    assert_eq!(independent.module().funcs.decoded_count(), 0);
    funcs.prepare_for_fork();
    assert_eq!(funcs.specialized_count(), 2);
    // Republishing retains templates even after this Engine has specialized both instances.
    let republished = load(&engine.shared().module, &Instance::default()).unwrap();
    let fresh = republished.instantiate().unwrap();
    assert_eq!(fresh.module().funcs.specialized_count(), 0);
    assert!(fresh.module().funcs.execution(0).unwrap().shared.is_some());
}

#[test]
fn generic_shared_archives_reject_invalid_descriptors_without_specializing() {
    for bad in 0..7 {
        let mut body = test_generic_body(11);
        let shared = body.shared.as_mut().unwrap();
        let mut code = GenericInstructions::clone(&shared.template);
        match bad {
            0 => shared.values.clear(),
            1 => code.dictionary = body.frame_size,
            2 => shared.locals.push(FrameLocal {
                pointer: Slot {
                    off: 0,
                    width: Width::W64,
                },
                off: 24,
                size: 8,
                align: 8,
            }),
            3 => shared.calls.push(Terminator::Return),
            4 => {
                code.blocks[0].term = Terminator::Call {
                    callee: 7,
                    args: Vec::new(),
                    ret: RetDest::Ignore,
                    target: 0,
                    unwind: UnwindAction::Continue,
                    role: CallRole::Normal,
                }
            }
            5 => code.blocks[0].stmts.push(Stmt::Assign {
                dst: ScalarPlace::Slot(Slot {
                    off: 16,
                    width: Width::W64,
                }),
                rv: Rvalue::Use(Operand::Imm {
                    bits: 0,
                    width: Width::W64,
                }),
            }),
            6 => code.blocks[0].stmts.push(Stmt::Assign {
                dst: ScalarPlace::Slot(Slot {
                    off: 8,
                    width: Width::W64,
                }),
                rv: Rvalue::TlsRef(0),
            }),
            _ => unreachable!(),
        }
        shared.template = Arc::new(GenericTemplate::new(code).unwrap());
        let mut module = Module {
            funcs: vec![body].into(),
            ..Module::default()
        };
        module.ensure_function_names();
        let instance = Instance::default();
        let owned = crate::vm::verify::module(&module, &instance).unwrap_err();
        let archived = load(&module, &instance)
            .err()
            .expect("invalid shared descriptor accepted");
        assert_eq!(owned.to_string(), archived.to_string(), "case {bad}");
        assert_eq!(module.funcs.specialized_count(), 0);
    }
}

#[test]
fn header_queries_preserve_lazy_bodies_across_archive_parts() {
    let slot = |off, width| Slot { off, width };
    let lo = slot(0, Width::W8);
    let hi = slot(8, Width::W64);
    let returns = [
        RetAbi::Zst,
        RetAbi::Scalar(lo),
        RetAbi::Pair(lo, hi),
        RetAbi::Indirect {
            ret_off: 0,
            size: 16,
            sret_off: 16,
        },
    ];
    let bodies: Vec<_> = returns
        .into_iter()
        .enumerate()
        .map(|(i, ret)| FuncBody {
            shared: None,
            prepared: Default::default(),
            objects: Vec::new(),
            frame_size: 128,
            frame_align: 64,
            ret,
            params: vec![
                ParamAbi::Zst,
                ParamAbi::Scalar(lo),
                ParamAbi::Pair(lo, hi),
                ParamAbi::Indirect { off: 32, size: 24 },
            ],
            caller_loc_off: (i % 2 != 0).then_some(64),
            name: format!("header{i}").into(),
            blocks: vec![Block {
                stmts: Vec::new(),
                term: Terminator::Return,
            }],
        })
        .collect();
    let archived = Module {
        funcs: bodies.clone().into(),
        ..Module::default()
    };
    let mut funcs = FuncTable::default();
    funcs.append(FuncTable::from_archive(Arc::new(archive(&archived))));
    funcs.append(bodies.clone().into());
    funcs.append(FuncTable::from_archive(Arc::new(archive(&archived))));
    let fields = |header: FuncHeader<'_>| {
        postcard::to_stdvec(&(
            header.frame_size(),
            header.frame_align(),
            header.ret(),
            header.params().collect::<Vec<_>>(),
            header.caller_loc_off(),
            header.name(),
        ))
        .unwrap()
    };
    for index in 0..funcs.len() {
        assert_eq!(
            fields(funcs.header(index).unwrap()),
            fields(FuncHeader::Owned(&bodies[index % bodies.len()]))
        );
    }
    assert!(funcs.header(funcs.len()).is_none());
    assert!(funcs.header(usize::MAX).is_none());
    assert_eq!(funcs.decoded_count(), bodies.len());
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

#[test]
fn single_use_address_checks_match_reusable_images_and_rebinding() {
    use crate::os_arch::addrspace::DELTA_FIXED_ADDR;
    use crate::vm::frozen::FrozenSnapshot;

    for address in [0, 17, DELTA_FIXED_ADDR as u64] {
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
            rv: Rvalue::Use(Operand::AddrImm(LinkAddr(address))),
        });
        module.funcs = bodies.into();
        let mut instance = Instance::materialize_dynamic(&module).unwrap();
        instance.load_map.require_mapped();
        let owned =
            crate::vm::verify::module(&module, &instance).map_err(|error| error.to_string());
        let single = load_once(&module, &instance)
            .map(|artifact| assert_eq!(artifact.module().funcs.decoded_count(), 0))
            .map_err(|error| error.to_string());
        let reusable = load(&module, &instance);
        assert_eq!(owned, single, "address={address:#x}");
        assert_eq!(
            owned,
            reusable
                .as_ref()
                .map(|_| ())
                .map_err(|error| error.to_string()),
            "address={address:#x}"
        );
        if let Ok(image) = reusable {
            image.instantiate().unwrap();
            let mut unrelated = Instance::default();
            unrelated.load_map.require_mapped();
            assert!(
                image
                    .addresses
                    .check(&module, &unrelated, module.funcs.len())
                    .is_err()
            );
        }
    }
}

#[test]
fn prepared_operations_are_verified_against_archived_semantics() {
    for bad in 0..5 {
        let mut module = module();
        let mut bodies = Vec::new();
        module.funcs.drain_into(&mut bodies);
        let body = &mut bodies[0];
        body.blocks[0].stmts.push(Stmt::Assign {
            dst: ScalarPlace::Slot(Slot {
                off: 0,
                width: Width::W64,
            }),
            rv: Rvalue::Use(Operand::Imm {
                bits: 17,
                width: Width::W64,
            }),
        });
        body.prepared = Prepared::new(&body.blocks, None);
        match bad {
            0 => {}
            1 => body.prepared.ops[0].dst = 8,
            2 => body.prepared.constants[0] = 18,
            3 => body.prepared.offsets[1] = 0,
            4 => body.prepared.ops[0].code = 255,
            _ => unreachable!(),
        }
        module.funcs = bodies.into();
        let instance = Instance::default();
        let owned = crate::vm::verify::module(&module, &instance).map_err(|e| e.to_string());
        let archived = load_once(&module, &instance)
            .map(|_| ())
            .map_err(|e| e.to_string());
        assert_eq!(owned, archived);
        assert_eq!(owned.is_ok(), bad == 0);
    }
}
