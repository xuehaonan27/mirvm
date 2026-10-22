//! Shared instructions and the concrete dictionary, frame and call bindings of one instance.

use std::sync::Arc;

use crate::vm::ir::*;

#[derive(
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct SharedBody {
    pub template: Arc<GenericTemplate>,
    pub values: Vec<u64>,
    /// Pointer slot and concrete storage offset, both relative to the activation's frame.
    pub locals: Vec<FrameLocal>,
    /// Concrete call adapters, indexed by the template's Call.callee ordinal.
    pub calls: Vec<Terminator>,
}

#[derive(Clone, Debug, serde::Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[archive(check_bytes)]
#[serde(transparent)]
pub struct GenericTemplate {
    #[serde(skip)]
    identity: [u8; 32],
    code: GenericInstructions,
}

/// Template content before its immutable content identity is assigned.
#[derive(
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct GenericInstructions {
    pub dictionary: u32,
    pub binding_count: u32,
    pub blocks: Vec<Block>,
    pub prepared: Prepared,
}

impl GenericTemplate {
    pub fn new(mut code: GenericInstructions) -> Result<Self, crate::error::Error> {
        code.prepared = Prepared::new(&code.blocks, Some(code.dictionary));
        let mut template = Self {
            identity: [0; 32],
            code,
        };
        template.identity =
            crate::vm::ir::frag::id_of(&crate::vm::ir::frag::encode_template(&template)?);
        Ok(template)
    }

    /// Content identity of the versioned template fragment. Bindings and concrete ABIs are separate.
    pub fn identity(&self) -> [u8; 32] {
        self.identity
    }
}

impl std::ops::Deref for GenericTemplate {
    type Target = GenericInstructions;

    fn deref(&self) -> &Self::Target {
        &self.code
    }
}

impl std::ops::Deref for ArchivedGenericTemplate {
    type Target = ArchivedGenericInstructions;

    fn deref(&self) -> &Self::Target {
        &self.code
    }
}

impl ArchivedGenericTemplate {
    /// Read the identity covered by the enclosing product's integrity and generation checks.
    pub fn identity(&self) -> [u8; 32] {
        self.identity
    }
}

impl<'de> serde::Deserialize<'de> for GenericTemplate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = serde::Deserialize::deserialize(deserializer)?;
        Self::new(code).map_err(serde::de::Error::custom)
    }
}

#[derive(
    Clone,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
#[archive(check_bytes)]
pub struct FrameLocal {
    pub pointer: Slot,
    pub off: u32,
    pub size: u32,
    pub align: u32,
}

impl FuncBody {
    pub(crate) fn code(&self) -> &[Block] {
        self.shared
            .as_ref()
            .map_or(&self.blocks, |shared| &shared.template.blocks)
    }

    pub(crate) fn term<'a>(&'a self, block: &'a Block) -> &'a Terminator {
        if let Some(shared) = &self.shared
            && let Terminator::Call { callee, .. } = &block.term
        {
            &shared.calls[*callee as usize]
        } else {
            &block.term
        }
    }

    /// Generate concrete instructions only when a consumer requests that execution form.
    /// The ABI and frame remain identical to the shared activation, including pointer slots.
    pub(crate) fn specialize(&self) -> Self {
        let Some(shared) = &self.shared else {
            return self.clone();
        };
        let mut body = self.clone();
        body.shared = None;
        body.blocks = shared.template.blocks.clone();
        let bind = |operand: &mut Operand| {
            if let Operand::Mem { expr, width } = operand
                && matches!(expr.base, PlaceBase::Local(off) if off == shared.template.dictionary)
            {
                let [PlaceStep::Deref, PlaceStep::Offset(off)] = &*expr.steps else {
                    unreachable!("verified dictionary addressing")
                };
                *operand = Operand::Imm {
                    bits: shared.values[*off as usize / 8],
                    width: *width,
                };
            }
        };
        for block in &mut body.blocks {
            for stmt in &mut block.stmts {
                match stmt {
                    Stmt::Assign { rv, .. } => match rv {
                        Rvalue::Use(a) | Rvalue::Cast { a, .. } => bind(a),
                        Rvalue::IntBin { a, b, .. } | Rvalue::IntCmp { a, b, .. } => {
                            bind(a);
                            bind(b);
                        }
                        Rvalue::Ref(_) => {}
                        _ => unreachable!("verified shared rvalue"),
                    },
                    Stmt::AssignOverflow { a, b, .. } => {
                        bind(a);
                        bind(b);
                    }
                    Stmt::MemCopy {
                        src, dst, count, ..
                    } => {
                        bind(src);
                        bind(dst);
                        bind(count);
                    }
                    _ => unreachable!("verified shared statement"),
                }
            }
            if let Terminator::Call { callee, .. } = &block.term {
                block.term = shared.calls[*callee as usize].clone();
            }
            match &mut block.term {
                Terminator::SwitchInt {
                    discr: SwitchDiscr::Scalar(a),
                    ..
                } => bind(a),
                Terminator::Call { args, .. } => args.iter_mut().for_each(&bind),
                _ => {}
            }
        }
        let mut initialize: Vec<_> = shared
            .locals
            .iter()
            .map(|local| Stmt::Assign {
                dst: ScalarPlace::Slot(local.pointer),
                rv: Rvalue::Ref(PlaceExpr {
                    base: PlaceBase::Local(local.off),
                    steps: Box::new([]),
                }),
            })
            .collect();
        initialize.append(&mut body.blocks[0].stmts);
        body.blocks[0].stmts = initialize;
        body.prepared = Prepared::new(&body.blocks, None);
        body
    }
}

impl ArchivedFuncBody {
    pub(crate) fn code(&self) -> &[ArchivedBlock] {
        self.shared
            .as_ref()
            .map_or(self.blocks.as_slice(), |shared| {
                shared.template.blocks.as_slice()
            })
    }

    pub(crate) fn term<'a>(&'a self, block: &'a ArchivedBlock) -> &'a ArchivedTerminator {
        if let Some(shared) = self.shared.as_ref()
            && let ArchivedTerminator::Call { callee, .. } = &block.term
        {
            &shared.calls[*callee as usize]
        } else {
            &block.term
        }
    }
}

#[cfg(test)]
pub(crate) fn test_generic_body(value: u64) -> FuncBody {
    let slot = |off| Slot {
        off,
        width: Width::W64,
    };
    FuncBody {
        prepared: Default::default(),
        objects: Vec::new(),
        frame_size: 24,
        frame_align: 8,
        ret: RetAbi::Scalar(slot(8)),
        params: vec![ParamAbi::Scalar(slot(0))],
        caller_loc_off: None,
        name: format!("generic{value}").into(),
        blocks: Vec::new(),
        shared: Some(Box::new(SharedBody {
            template: Arc::new(
                GenericTemplate::new(GenericInstructions {
                    prepared: Default::default(),
                    dictionary: 16,
                    binding_count: 1,
                    blocks: vec![Block {
                        stmts: vec![Stmt::Assign {
                            dst: ScalarPlace::Slot(slot(8)),
                            rv: Rvalue::IntBin {
                                op: IntBinOp::Add,
                                signed: false,
                                a: Operand::Slot(slot(0)),
                                b: Operand::Mem {
                                    expr: PlaceExpr {
                                        base: PlaceBase::Local(16),
                                        steps: vec![PlaceStep::Deref, PlaceStep::Offset(0)].into(),
                                    },
                                    width: Width::W64,
                                },
                            },
                        }],
                        term: Terminator::Return,
                    }],
                })
                .unwrap(),
            ),
            values: vec![value],
            locals: Vec::new(),
            calls: Vec::new(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_overflow_binds_constants_and_protects_dictionary() {
        let mut body = test_generic_body(7);
        body.frame_size = 32;
        let shared = body.shared.as_mut().unwrap();
        let mut code = GenericInstructions::clone(&shared.template);
        let Stmt::Assign {
            dst,
            rv: Rvalue::IntBin { a, b, .. },
        } = code.blocks[0].stmts.remove(0)
        else {
            unreachable!();
        };
        code.blocks[0].stmts.push(Stmt::AssignOverflow {
            op: OvfOp::Add,
            signed: false,
            a,
            b,
            dst_val: dst,
            dst_flag: ScalarPlace::Slot(Slot {
                off: 24,
                width: Width::W8,
            }),
        });
        shared.template = Arc::new(GenericTemplate::new(code.clone()).unwrap());
        let verify = |body: FuncBody| {
            let module = Module {
                funcs: FuncTable::from(vec![body]),
                ..Module::default()
            };
            crate::vm::verify::module(&module, &crate::vm::instance::Instance::default())
        };
        verify(body.clone()).unwrap();
        let concrete = body.specialize();
        assert!(matches!(
            concrete.blocks[0].stmts[0],
            Stmt::AssignOverflow {
                b: Operand::Imm { bits: 7, .. },
                ..
            }
        ));
        verify(concrete).unwrap();
        for flag in [false, true] {
            let mut damaged = code.clone();
            let Stmt::AssignOverflow {
                dst_val, dst_flag, ..
            } = &mut damaged.blocks[0].stmts[0]
            else {
                unreachable!();
            };
            *(if flag { dst_flag } else { dst_val }) = ScalarPlace::Slot(Slot {
                off: 16,
                width: Width::W64,
            });
            body.shared.as_mut().unwrap().template =
                Arc::new(GenericTemplate::new(damaged).unwrap());
            assert!(verify(body.clone()).is_err());
        }
    }

    #[test]
    fn generic_template_identity_preserves_fragment_encoding_and_tracks_content() {
        let body = test_generic_body(11);
        let template = &body.shared.as_ref().unwrap().template;
        let code = GenericInstructions::clone(template);
        let old_payload = postcard::to_stdvec(&code).unwrap();
        assert_eq!(postcard::to_stdvec(template).unwrap(), old_payload);
        let mut fragment = vec![frag::ENCODING_VERSION, b'T'];
        fragment.extend(old_payload);
        assert_eq!(frag::encode_template(template).unwrap(), fragment);
        assert_eq!(template.identity(), frag::id_of(&fragment));
        let decoded = frag::decode_template(&fragment).unwrap();
        assert_eq!(decoded.identity(), template.identity());
        let other = test_generic_body(29);
        assert_eq!(
            other.shared.unwrap().template.identity(),
            template.identity()
        );
        for field in 0..3 {
            let mut changed = code.clone();
            match field {
                0 => changed.dictionary += 8,
                1 => changed.binding_count += 1,
                2 => changed.blocks[0].term = Terminator::Unreachable,
                _ => unreachable!(),
            }
            assert_ne!(
                GenericTemplate::new(changed).unwrap().identity(),
                template.identity()
            );
        }
    }
}
