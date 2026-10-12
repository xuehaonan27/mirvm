//! Bounded generic MIR export. Unsupported syntax fails before an artifact is written.

use super::*;
use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::Compiler;
use rustc_middle::{
    mir,
    mono::MonoItem,
    ty::{self, TyCtxt, TypingEnv},
};

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Export {
    pub template: FuncBody,
    pub descriptors: Vec<Descriptor>,
    pub symbols: Vec<String>,
    pub export_ns: u128,
    pub descriptors_ns: u128,
    pub validation_ns: u128,
}

struct Exporter<'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'tcx mir::Body<'tcx>,
    dictionary: u32,
    temporary: u32,
}

impl<'tcx> Exporter<'tcx> {
    fn slot(&self, place: mir::Place<'tcx>) -> Slot {
        assert!(place.projection.is_empty(), "unsupported place projection");
        let ty = self.body.local_decls[place.local].ty;
        Slot {
            off: place.local.as_u32() * 8,
            width: if ty.is_bool() { Width::W8 } else { Width::W64 },
        }
    }
    fn binding(&self, index: u32) -> Operand {
        let mut value = binding(index);
        if let Operand::Mem { expr, .. } = &mut value {
            expr.base = PlaceBase::Local(self.dictionary);
        }
        value
    }
    fn operand(&self, operand: &mir::Operand<'tcx>) -> Operand {
        match operand {
            mir::Operand::Copy(p) | mir::Operand::Move(p) => Operand::Slot(self.slot(*p)),
            mir::Operand::Constant(c) => {
                if let mir::Const::Ty(_, value) = c.const_
                    && let ty::ConstKind::Param(param) = value.kind()
                {
                    assert_eq!(param.index, 1, "only the declared const length is admitted");
                    return self.binding(1);
                }
                let value = c
                    .const_
                    .try_eval_scalar_int(self.tcx, TypingEnv::fully_monomorphized())
                    .expect("unsupported constant");
                Operand::Imm {
                    bits: u64::try_from(value.to_bits(value.size())).unwrap(),
                    width: if c.const_.ty().is_bool() {
                        Width::W8
                    } else {
                        Width::W64
                    },
                }
            }
            _ => panic!("unsupported operand"),
        }
    }
    fn export(&self) -> FuncBody {
        assert_eq!(self.tcx.data_layout.pointer_size().bytes(), 8);
        assert_eq!(self.body.arg_count, 3);
        for decl in self.body.local_decls.iter() {
            assert!(
                matches!(
                    decl.ty.kind(),
                    ty::Bool
                        | ty::Uint(ty::UintTy::Usize)
                        | ty::Int(ty::IntTy::Isize)
                        | ty::RawPtr(_, _)
                ),
                "unsupported local type: {}",
                decl.ty
            );
            if let ty::RawPtr(pointee, _) = decl.ty.kind() {
                assert!(
                    matches!(pointee.kind(), ty::Param(param) if param.index == 0),
                    "unsupported pointee"
                );
            }
        }
        for index in [0, 3] {
            assert!(
                self.body.local_decls[mir::Local::from_usize(index)]
                    .ty
                    .is_usize()
            );
        }
        assert!(
            self.body.local_decls[mir::Local::from_usize(1)]
                .ty
                .is_raw_ptr()
        );
        assert!(
            self.body.local_decls[mir::Local::from_usize(2)]
                .ty
                .is_raw_ptr()
        );
        let mut blocks = Vec::new();
        for block in self.body.basic_blocks.iter() {
            assert!(!block.is_cleanup, "cleanup requires the witness exporter");
            let mut stmts = Vec::new();
            for stmt in &block.statements {
                match &stmt.kind {
                    mir::StatementKind::Assign(box (place, rv)) => {
                        let rv = match rv {
                            mir::Rvalue::Use(value, _) => Rvalue::Use(self.operand(value)),
                            mir::Rvalue::Cast(kind, value, _) => {
                                assert!(matches!(
                                    kind,
                                    mir::CastKind::IntToInt | mir::CastKind::PtrToPtr
                                ));
                                Rvalue::Use(self.operand(value))
                            }
                            mir::Rvalue::BinaryOp(op, box (a, b)) => match op {
                                mir::BinOp::Add => Rvalue::IntBin {
                                    op: IntBinOp::Add,
                                    signed: false,
                                    a: self.operand(a),
                                    b: self.operand(b),
                                },
                                mir::BinOp::Lt => {
                                    assert!(
                                        a.ty(&self.body.local_decls, self.tcx).is_usize(),
                                        "signed comparisons need a signed binding"
                                    );
                                    Rvalue::IntCmp {
                                        cc: IntCc::Lt,
                                        signed: false,
                                        a: self.operand(a),
                                        b: self.operand(b),
                                    }
                                }
                                _ => panic!("unsupported binary operation: {op:?}"),
                            },
                            _ => panic!("unsupported rvalue: {rv:?}"),
                        };
                        stmts.push(Stmt::Assign {
                            dst: ScalarPlace::Slot(self.slot(*place)),
                            rv,
                        });
                    }
                    mir::StatementKind::Intrinsic(
                        box mir::NonDivergingIntrinsic::CopyNonOverlapping(copy),
                    ) => {
                        stmts.push(bin(
                            self.temporary,
                            IntBinOp::Mul,
                            self.operand(&copy.count),
                            self.binding(0),
                        ));
                        stmts.push(Stmt::MemCopy {
                            dst: self.operand(&copy.dst),
                            src: self.operand(&copy.src),
                            count: local(self.temporary),
                            elem_size: 1,
                            overlap: false,
                        });
                    }
                    mir::StatementKind::StorageLive(_)
                    | mir::StatementKind::StorageDead(_)
                    | mir::StatementKind::Nop => {}
                    other => panic!("unsupported statement: {other:?}"),
                }
            }
            let term = match &block.terminator().kind {
                mir::TerminatorKind::Goto { target } => Terminator::Goto(target.as_u32()),
                mir::TerminatorKind::Return => Terminator::Return,
                mir::TerminatorKind::SwitchInt { discr, targets } => Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(self.operand(discr)),
                    targets: targets
                        .iter()
                        .map(|(value, target)| (value, target.as_u32()))
                        .collect(),
                    otherwise: targets.otherwise().as_u32(),
                },
                mir::TerminatorKind::Call {
                    func,
                    args,
                    destination,
                    target: Some(target),
                    ..
                } => {
                    let ty::FnDef(def, _) = *func.ty(&self.body.local_decls, self.tcx).kind()
                    else {
                        panic!("indirect MIR call")
                    };
                    let intrinsic = self
                        .tcx
                        .intrinsic(def)
                        .expect("non-intrinsic calls require the witness exporter");
                    assert_eq!(intrinsic.name.as_str(), "arith_offset");
                    assert_eq!(args.len(), 2);
                    stmts.push(bin(
                        self.temporary,
                        IntBinOp::Mul,
                        self.operand(&args[1].node),
                        self.binding(0),
                    ));
                    stmts.push(Stmt::Assign {
                        dst: ScalarPlace::Slot(self.slot(*destination)),
                        rv: Rvalue::IntBin {
                            op: IntBinOp::Add,
                            signed: false,
                            a: self.operand(&args[0].node),
                            b: local(self.temporary),
                        },
                    });
                    Terminator::Goto(target.as_u32())
                }
                other => panic!("unsupported terminator: {other:?}"),
            };
            blocks.push(Block { stmts, term });
        }
        let mut out = body("exported_template", blocks, false);
        out.frame_size = self.temporary + 8;
        out.params.push(ParamAbi::Scalar(slot(self.dictionary)));
        out
    }
}

struct Frontend {
    output: Option<Export>,
}
impl Callbacks for Frontend {
    fn after_analysis<'tcx>(&mut self, _: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        let started = Instant::now();
        let partition = tcx.collect_and_partition_mono_items(());
        tcx.dcx().abort_if_errors();
        let validation_ns = started.elapsed().as_nanos();
        let def = tcx
            .hir_crate_items(())
            .definitions()
            .find(|id| {
                tcx.opt_item_name(id.to_def_id())
                    .is_some_and(|s| s.as_str() == "copy_elements")
            })
            .expect("copy_elements definition")
            .to_def_id();
        let started = Instant::now();
        let generic = tcx.optimized_mir(def);
        let dictionary = generic.local_decls.len() as u32 * 8;
        let template = Exporter {
            tcx,
            body: generic,
            dictionary,
            temporary: dictionary + 8,
        }
        .export();
        let export_ns = started.elapsed().as_nanos();
        let started = Instant::now();
        let mut instances = std::collections::HashMap::new();
        for cgu in partition.codegen_units {
            for item in cgu.items().keys() {
                if let MonoItem::Fn(instance) = item
                    && instance.def_id() == def
                {
                    let ty = instance.args.type_at(0);
                    let layout = tcx
                        .layout_of(TypingEnv::fully_monomorphized().as_query_input(ty))
                        .unwrap();
                    let length = instance.args.const_at(1).try_to_target_usize(tcx).unwrap();
                    let descriptor = Descriptor {
                        stride: layout.size.bytes(),
                        align: layout.align.abi.bytes(),
                        length,
                        kernel: Kernel::Array,
                    };
                    descriptor.validate();
                    instances.insert(tcx.symbol_name(*instance).name.to_string(), descriptor);
                }
            }
        }
        assert!(!instances.is_empty());
        let mut instances: Vec<_> = instances.into_iter().collect();
        instances.sort_by(|a, b| a.0.cmp(&b.0));
        let (symbols, descriptors) = instances.into_iter().unzip();
        self.output = Some(Export {
            template,
            descriptors,
            symbols,
            export_ns,
            descriptors_ns: started.elapsed().as_nanos(),
            validation_ns,
        });
        Compilation::Stop
    }
}

pub(super) fn run(args: &[String]) {
    assert!(
        args.len() >= 2,
        "export output.json source.rs [rustc options]"
    );
    let mut frontend = Frontend { output: None };
    let mut compiler_args = vec![
        "rustc".into(),
        args[1].clone(),
        "--edition=2024".into(),
        "-Copt-level=0".into(),
        "-Coverflow-checks=off".into(),
        "-Zmir-opt-level=1".into(),
    ];
    compiler_args.extend_from_slice(&args[2..]);
    let code = rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&compiler_args, &mut frontend)
    });
    if code != std::process::ExitCode::SUCCESS {
        std::process::exit(1);
    }
    // The callback's result is fully owned; publication follows compiler diagnostics and teardown.
    let output = frontend.output.expect("analysis did not export a template");
    std::fs::write(&args[0], serde_json::to_vec_pretty(&output).unwrap()).unwrap();
}
