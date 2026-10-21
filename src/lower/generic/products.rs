//! Consumer-independent unbound products. Compiler types and spans are recovered from checked MIR
//! recipe locations; the store never serializes a compiler allocation or a session-local DefId.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rustc_hir::def_id::{CrateNum, DefId};
use rustc_middle::{
    mir,
    ty::{self, Instance, Ty, TyCtxt},
};
use serde::{Deserialize, Serialize};

use super::template::{Binding, CallAbi, LocalBinding, Template};
use super::{Call, Export};
use crate::vm::archive::{bytes::Bytes, functions};
use crate::vm::ir::FuncTable;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    crate_id: u64,
    crate_hash: String,
    target: String,
    features: String,
    cpu: Option<String>,
    checks: (bool, bool, bool),
}

impl Identity {
    fn new(tcx: TyCtxt<'_>, krate: CrateNum) -> Self {
        Self {
            crate_id: tcx.stable_crate_id(krate).as_u64(),
            // rustc includes upstream crate identities, source contents and tracked producer flags.
            crate_hash: tcx.crate_hash(krate).to_hex(),
            target: format!("{:?}", tcx.sess.target),
            features: tcx.sess.opts.cg.target_feature.clone(),
            cpu: tcx.sess.opts.cg.target_cpu.clone(),
            checks: (
                tcx.sess.ub_checks(),
                tcx.sess.overflow_checks(),
                tcx.sess.contract_checks(),
            ),
        }
    }

    fn path(&self) -> PathBuf {
        let mut bytes = crate::options::build::BUILD_ID.as_bytes().to_vec();
        bytes.extend(postcard::to_stdvec(self).expect("owned template identity"));
        crate::store::GENERIC_TEMPLATES
            .dir()
            .join(format!("{}.bin", blake3::hash(&bytes)))
    }
}

/// TypeWalker order is fixed by the pinned compiler and the defining crate's exact MIR.
#[derive(Clone, Serialize, Deserialize)]
struct LayoutRecipe {
    local: u32,
    ty: u32,
}

#[derive(Clone, Serialize, Deserialize)]
struct Recipe {
    body: usize,
    bindings: Vec<Binding>,
    locals: Vec<LocalBinding>,
    calls: Vec<CallAbi>,
    layouts: Vec<LayoutRecipe>,
    callees: Vec<u32>,
}

#[derive(Serialize, Deserialize)]
struct Header {
    build_id: String,
    identity: Identity,
    records: BTreeMap<String, Option<Recipe>>,
}

pub(super) struct Products {
    identity: Identity,
    path: PathBuf,
    records: BTreeMap<String, Option<Recipe>>,
    bodies: FuncTable,
    pending: BTreeMap<String, Option<Recipe>>,
}

fn definition(tcx: TyCtxt<'_>, def: DefId) -> String {
    format!("{:?}", tcx.def_path_hash(def))
}

impl Products {
    pub fn open(tcx: TyCtxt<'_>, krate: CrateNum) -> Self {
        let identity = Identity::new(tcx, krate);
        let path = identity.path();
        Self::read(&path, &identity).unwrap_or(Self {
            identity,
            path,
            records: BTreeMap::new(),
            bodies: FuncTable::default(),
            pending: BTreeMap::new(),
        })
    }

    fn read(path: &Path, identity: &Identity) -> Option<Self> {
        Self::decode(path.to_owned(), identity, std::fs::read(path).ok()?)
    }

    fn decode(path: PathBuf, identity: &Identity, bytes: Vec<u8>) -> Option<Self> {
        let end = bytes.len().checked_sub(32)?;
        if blake3::hash(&bytes[..end]).as_bytes() != &bytes[end..] {
            return None;
        }
        let (header, rest) = postcard::take_from_bytes::<Header>(&bytes[..end]).ok()?;
        if header.build_id != crate::options::build::BUILD_ID || header.identity != *identity {
            return None;
        }
        let start = (end - rest.len()).checked_next_multiple_of(rkyv::AlignedVec::ALIGNMENT)?;
        let archive = functions::Functions::load(Bytes::new(bytes), start..end).ok()?;
        if header
            .records
            .values()
            .flatten()
            .any(|recipe| recipe.body >= archive.len())
        {
            return None;
        }
        Some(Self {
            identity: header.identity,
            path,
            records: header.records,
            bodies: FuncTable::from_archive(std::sync::Arc::new(archive)),
            pending: BTreeMap::new(),
        })
    }

    /// The outer option is a cache hit; the inner option records export admission.
    pub fn get<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        instance: Instance<'tcx>,
    ) -> Option<Option<Export<'tcx>>> {
        let Some(recipe) = self.records.get(&definition(tcx, instance.def_id()))? else {
            return Some(None);
        };
        let mir = tcx.instance_mir(instance.def);
        let mut body = self.bodies.execution(recipe.body)?.clone();
        let shared = body.shared.take()?;
        let template = Template {
            body,
            instructions: shared.template,
            bindings: recipe.bindings.clone(),
            locals: recipe.locals.clone(),
            calls: recipe.calls.clone(),
        };
        template.validate(
            recipe.layouts.len(),
            instance.args.len(),
            mir.local_decls.len(),
        )?;
        if template.body.params.len() != mir.arg_count + 1
            || recipe.callees.len() != template.calls.len()
        {
            return None;
        }
        let layouts = recipe
            .layouts
            .iter()
            .map(|recipe| {
                mir.local_decls
                    .get(mir::Local::from_u32(recipe.local))?
                    .ty
                    .walk()
                    .nth(recipe.ty as usize)?
                    .as_type()
            })
            .collect::<Option<Vec<_>>>()?;
        let calls = recipe
            .callees
            .iter()
            .map(|&block| restore_call(tcx, mir, block))
            .collect::<Option<Vec<_>>>()?;
        Some(Some(Export {
            template,
            layouts,
            calls,
        }))
    }

    pub fn insert<'tcx>(
        &mut self,
        tcx: TyCtxt<'tcx>,
        instance: Instance<'tcx>,
        export: Option<&Export<'tcx>>,
    ) {
        let key = definition(tcx, instance.def_id());
        let recipe = match export {
            Some(export) => {
                let mir = tcx.instance_mir(instance.def);
                let Some(layouts) = export
                    .layouts
                    .iter()
                    .map(|&ty| layout_recipe(mir, ty))
                    .collect::<Option<Vec<_>>>()
                else {
                    return;
                };
                let Some(body) = export.template.archive_body() else {
                    return;
                };
                let recipe = Recipe {
                    body: self.bodies.len(),
                    bindings: export.template.bindings.clone(),
                    locals: export.template.locals.clone(),
                    calls: export.template.calls.clone(),
                    layouts,
                    callees: export.calls.iter().map(|call| call.block).collect(),
                };
                self.bodies.append(FuncTable::from(vec![body]));
                Some(recipe)
            }
            None => None,
        };
        self.pending.insert(key.clone(), recipe.clone());
        self.records.insert(key, recipe);
    }

    pub fn publish(&mut self) -> Result<(), super::super::Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let dir = self.path.parent().expect("template family");
        let _lock = crate::store::frags::Lock::exclusive(dir)
            .map_err(|error| super::super::Error::io("cannot lock generic templates", error))?;
        let mut latest = Self::read(&self.path, &self.identity).unwrap_or(Self {
            identity: self.identity.clone(),
            path: self.path.clone(),
            records: BTreeMap::new(),
            bodies: FuncTable::default(),
            pending: BTreeMap::new(),
        });
        let mut added = Vec::new();
        let mut replaced = false;
        for (key, recipe) in &self.pending {
            let recipe = recipe.as_ref().map(|recipe| {
                let mut next = recipe.clone();
                next.body = latest.bodies.len() + added.len();
                added.push(
                    self.bodies
                        .execution(recipe.body)
                        .expect("inserted template")
                        .clone(),
                );
                next
            });
            replaced |= matches!(latest.records.insert(key.clone(), recipe), Some(Some(_)));
        }
        latest.bodies.append(FuncTable::from(added));
        if replaced {
            // Concurrent exporters may prepare the same definition. Keep only referenced bodies
            // when merging those writes, so repeated publication cannot grow an archive forever.
            let mut bodies = Vec::new();
            for recipe in latest.records.values_mut().flatten() {
                let body = latest
                    .bodies
                    .execution(recipe.body)
                    .expect("merged recipe names a stored body")
                    .clone();
                recipe.body = bodies.len();
                bodies.push(body);
            }
            latest.bodies = bodies.into();
        }
        let bytes = latest.encode()?;
        crate::store::publish_bytes(&self.path, &bytes)
            .map_err(|error| super::super::Error::io("cannot publish generic templates", error))?;
        self.pending.clear();
        Ok(())
    }

    pub fn stamp(&self) -> Option<Result<crate::utils::content::FileStamp, super::super::Error>> {
        if self.records.is_empty() {
            return None;
        }
        Some(
            crate::utils::content::FileStamp::of(&self.path)
                .map_err(|error| super::super::Error::io("cannot stamp prepared templates", error)),
        )
    }

    fn encode(&self) -> Result<Vec<u8>, super::super::Error> {
        let header = Header {
            build_id: crate::options::build::BUILD_ID.into(),
            identity: self.identity.clone(),
            records: self.records.clone(),
        };
        let mut bytes = postcard::to_stdvec(&header).map_err(|error| {
            super::super::Error::internal(format!(
                "cannot encode generic template recipes: {error}"
            ))
        })?;
        bytes.resize(bytes.len().next_multiple_of(rkyv::AlignedVec::ALIGNMENT), 0);
        bytes.extend_from_slice(&functions::encode(&self.bodies).map_err(|error| {
            super::super::Error::internal(format!("cannot encode generic templates: {error}"))
        })?);
        let digest = blake3::hash(&bytes);
        bytes.extend_from_slice(digest.as_bytes());
        Ok(bytes)
    }
}

fn layout_recipe<'tcx>(mir: &mir::Body<'tcx>, wanted: Ty<'tcx>) -> Option<LayoutRecipe> {
    mir.local_decls.iter_enumerated().find_map(|(local, decl)| {
        decl.ty.walk().enumerate().find_map(|(index, arg)| {
            (arg.as_type() == Some(wanted)).then(|| {
                Some(LayoutRecipe {
                    local: local.as_u32(),
                    ty: u32::try_from(index).ok()?,
                })
            })?
        })
    })
}

fn restore_call<'tcx>(tcx: TyCtxt<'tcx>, mir: &mir::Body<'tcx>, block: u32) -> Option<Call<'tcx>> {
    let term = mir
        .basic_blocks
        .get(mir::BasicBlock::from_u32(block))?
        .terminator();
    let span = term.source_info.span;
    let (ty, location) = match &term.kind {
        mir::TerminatorKind::Call { func, fn_span, .. } => {
            let ty = func.ty(&mir.local_decls, tcx);
            (
                ty,
                super::call_location(
                    tcx,
                    mir,
                    ty,
                    mir::SourceInfo {
                        span: *fn_span,
                        ..term.source_info
                    },
                ),
            )
        }
        mir::TerminatorKind::Drop { place, .. } => (
            Ty::new_fn_def(
                tcx,
                tcx.require_lang_item(rustc_hir::LangItem::DropGlue, span),
                [place.ty(&mir.local_decls, tcx).ty],
            ),
            None,
        ),
        mir::TerminatorKind::Assert { msg, .. } => {
            let panic = match &**msg {
                mir::AssertKind::BoundsCheck { .. } => rustc_hir::LangItem::PanicBoundsCheck,
                mir::AssertKind::Overflow(..) => msg.panic_function(),
                _ => return None,
            };
            (
                Ty::new_fn_def(
                    tcx,
                    tcx.require_lang_item(panic, span),
                    ty::GenericArgs::empty(),
                ),
                Some(mir.caller_location_span(term.source_info, None, tcx, |span| span)),
            )
        }
        _ => return None,
    };
    matches!(ty.kind(), ty::FnDef(..)).then_some(Call {
        ty,
        location,
        block,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::ir::*;

    fn identity() -> Identity {
        Identity {
            crate_id: 42,
            crate_hash: "contents-and-upstream".into(),
            target: "test-target".into(),
            features: String::new(),
            cpu: None,
            checks: (true, true, false),
        }
    }

    fn template() -> Template {
        let mut body = crate::vm::ir::test_generic_body(0);
        let shared = body.shared.take().unwrap();
        body.params.push(ParamAbi::Scalar(Slot {
            off: shared.template.dictionary,
            width: Width::W64,
        }));
        Template {
            body,
            instructions: shared.template,
            bindings: vec![Binding::Const(0, Width::W64)],
            locals: Vec::new(),
            calls: Vec::new(),
        }
    }

    fn products(path: PathBuf, key: &str) -> Products {
        let template = template();
        let recipe = Recipe {
            body: 0,
            bindings: template.bindings.clone(),
            locals: Vec::new(),
            calls: Vec::new(),
            layouts: Vec::new(),
            callees: Vec::new(),
        };
        let records = BTreeMap::from([(key.into(), Some(recipe)), ("rejected".into(), None)]);
        Products {
            identity: identity(),
            path,
            pending: records.clone(),
            records,
            bodies: vec![template.archive_body().unwrap()].into(),
        }
    }

    #[test]
    fn product_replay_checks_exact_identity_and_integrity_without_specializing() {
        let original = products(PathBuf::from("unused.bin"), "definition");
        let bytes = original.encode().unwrap();
        let replay = Products::decode(original.path.clone(), &identity(), bytes.clone()).unwrap();
        assert_eq!(replay.bodies.decoded_count(), 0);
        assert_eq!(replay.records.len(), 2);
        assert!(replay.records["rejected"].is_none());
        assert!(replay.bodies.execution(0).unwrap().shared.is_some());
        assert_eq!(replay.bodies.specialized_count(), 0);
        for change in 0..6 {
            let mut other = identity();
            match change {
                0 => other.crate_id += 1,
                1 => other.crate_hash.push('x'),
                2 => other.target.push('x'),
                3 => other.features.push('x'),
                4 => other.cpu = Some("different".into()),
                _ => other.checks.0 = !other.checks.0,
            }
            assert!(Products::decode(original.path.clone(), &other, bytes.clone()).is_none());
        }
        let mut corrupt = bytes.clone();
        corrupt[10] ^= 1;
        assert!(Products::decode(original.path.clone(), &identity(), corrupt).is_none());
        for len in [0, 31, bytes.len() - 1] {
            assert!(
                Products::decode(original.path.clone(), &identity(), bytes[..len].to_vec())
                    .is_none()
            );
        }
    }

    #[test]
    fn recipe_cannot_reference_a_missing_archived_body() {
        let mut original = products(PathBuf::from("unused.bin"), "definition");
        original
            .records
            .get_mut("definition")
            .unwrap()
            .as_mut()
            .unwrap()
            .body = 1;
        assert!(
            Products::decode(
                original.path.clone(),
                &identity(),
                original.encode().unwrap()
            )
            .is_none()
        );
    }

    #[test]
    fn unbound_schema_rejects_invalid_bindings_and_instructions() {
        let mut template = template();
        assert!(template.validate(0, 1, 2).is_some());
        template.bindings[0] = Binding::Size(0);
        assert!(template.validate(0, 1, 2).is_none());
        template.bindings[0] = Binding::Align(0);
        assert!(template.validate(0, 1, 2).is_none());
        template.bindings[0] = Binding::Const(1, Width::W64);
        assert!(template.validate(0, 1, 2).is_none());
        template.bindings[0] = Binding::Const(0, Width::W64);
        let mut code = crate::vm::ir::GenericInstructions::clone(&template.instructions);
        code.blocks[0].term = Terminator::Goto(99);
        template.instructions =
            std::sync::Arc::new(crate::vm::ir::GenericTemplate::new(code).unwrap());
        assert!(template.validate(0, 1, 2).is_none());
    }

    #[test]
    fn stale_publishers_merge_definitions_without_accumulating_replaced_bodies() {
        let dir = std::env::temp_dir().join(format!("mirvm-template-merge-{}", std::process::id()));
        let path = dir.join("products.bin");
        let mut first = products(path.clone(), "first");
        let mut second = products(path.clone(), "second");
        first.publish().unwrap();
        second.publish().unwrap();
        let mut duplicate = products(path.clone(), "first");
        duplicate.publish().unwrap();
        let merged = Products::read(&path, &identity()).unwrap();
        assert_eq!(merged.records.len(), 3);
        assert_eq!(merged.bodies.len(), 2);
        assert!(merged.records["rejected"].is_none());
        assert!(
            first.pending.is_empty() && second.pending.is_empty() && duplicate.pending.is_empty()
        );
        assert_eq!(merged.bodies.specialized_count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn publication_failure_keeps_pending_products_for_a_retry() {
        let dir =
            std::env::temp_dir().join(format!("mirvm-template-failure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let mut products = products(blocked.join("products.bin"), "definition");
        assert!(products.publish().is_err());
        assert_eq!(products.pending.len(), 2);
        std::fs::remove_file(&blocked).unwrap();
        products.publish().unwrap();
        assert!(Products::read(&products.path, &identity()).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
