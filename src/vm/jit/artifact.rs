//! One compiled symbol's machine code as a storable artifact, and the link that brings it back.
//!
//! An entry may not carry an address of the process that produced it. The code holds offsets, and
//! every absolute it refers to is a *name* this process can resolve again: the [`super::reloc`]
//! vocabulary for what the guest program owns, the import whitelist for what generated code may call,
//! and an offset for a reference the backend resolved to a label inside the same buffer. The artifact
//! is exactly that — the backend's machine code plus its relocation list in canonical form — and the
//! link is the same resolution run once more against live state.
//!
//! A kind this file does not apply — an AArch64 call, a GOT reference, a TLS descriptor — is a
//! *miss*, never a guess: the caller compiles instead. The kinds it does apply are applied exactly
//! as `cranelift-jit`'s in-process relocation applies them, so a linked symbol is byte-identical to
//! the one the module produced.

use cranelift_codegen::FinalizedRelocTarget;
use cranelift_codegen::binemit::Reloc as BackendReloc;
use cranelift_codegen::ir::{ExternalName, Function};
use cranelift_jit::JITModule;
use cranelift_module::Module;

use crate::vm::ir;

use super::reloc::{self, Site};
use super::state::{CodeDomain, JitSymbolRole};
use super::{Body, SigPart};

/// Encoding version, part of what an entry's bytes mean.
const VERSION: u8 = 1;

/// How one relocation's value lands in the code. Mirrors the backend kinds this engine replays.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Kind {
    /// A whole 4-byte absolute field.
    Abs32,
    /// A whole 8-byte absolute field.
    Abs64,
    /// A signed 4-byte field holding `value - place`.
    PcRel32,
}

impl Kind {
    /// The backend kind this one replays, or `None` for a kind this engine does not apply.
    fn of(kind: BackendReloc) -> Option<Kind> {
        match kind {
            BackendReloc::Abs4 => Some(Kind::Abs32),
            BackendReloc::Abs8 => Some(Kind::Abs64),
            BackendReloc::X86PCRel4 | BackendReloc::X86CallPCRel4 => Some(Kind::PcRel32),
            _ => None,
        }
    }
}

/// What a relocation's value is named by.
///
/// The names in the first group are *references of the fragment itself*, by the ordinal the canonical
/// walk gives them, or the entry's own function: the loading session answers them from the body it is
/// compiling, which is what makes one entry valid in any program that binds the same fragment.
/// `Body` names a location in that body, canonical for the same reason. `Named` and `Local` need no
/// body: a helper of the import whitelist (or another symbol of this entry), and an offset the backend
/// resolved to a label inside the symbol.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Target {
    /// The id of the function this entry compiles, as the slow-path helpers take it.
    OwnId,
    /// A function id the body names, by canonical ordinal, as the helpers take it.
    FuncId(u32),
    /// The address of the PLT slot a call goes through, for the function it names.
    Slot { domain: CodeDomain, func: Ref },
    /// The runtime address of an asm stub the body names, by canonical ordinal.
    Stub(u32),
    /// The runtime address of a frozen link address the body names, by canonical ordinal.
    Link(u32),
    /// A location inside the resident decoded body.
    Body(Body),
    /// A symbol of the import whitelist, or another symbol of this entry.
    Named(Box<str>),
    /// An offset inside this symbol's own code.
    Local(u32),
}

/// Which function a relocation names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Ref {
    /// The function this entry compiles.
    Own,
    /// A function the body names, by canonical ordinal.
    Ordinal(u32),
}

/// The canonical ordinals of one body: what each reference site of the fragment stands for, in the
/// numbering the fragment encoding gives it.
///
/// The JIT records a site as the ordinal its body carries, never as the value this session baked, so
/// the artifact is a function of the body alone. Two programs that bind the same fragment therefore
/// produce the same bytes, and the value the code needs is resolved again when the entry is linked.
#[derive(Default)]
pub(crate) struct Ordinals {
    fragment: [u8; 32],
    /// The canonical body's length, which is the size a decision about this body is priced on (the
    /// pre-linking floor, the ledger). Free here: the canonical bytes are what the id is taken over.
    size: u32,
    targets: Vec<ir::frag::Target>,
    by_target: std::collections::HashMap<ir::frag::Target, u32>,
}

impl Ordinals {
    /// Number one body's reference sites, exactly as [`ir::frag::canonical`] does, and take the
    /// fragment id over the same canonical bytes: the id is what an entry is keyed by, and a body whose
    /// bytes do not encode has no entry at all.
    pub(crate) fn of(body: &ir::FuncBody) -> Option<Ordinals> {
        let canonical = ir::frag::canonical(body);
        let bytes = ir::frag::encode_canonical(&canonical.body).ok()?;
        let targets = canonical.targets;
        Some(Ordinals {
            fragment: ir::frag::id_of(&bytes),
            size: u32::try_from(bytes.len()).ok()?,
            by_target: targets
                .iter()
                .enumerate()
                .map(|(ordinal, target)| (*target, ordinal as u32))
                .collect(),
            targets,
        })
    }

    /// The fragment this body *is*, which is the entry's semantic key.
    pub(crate) fn fragment(&self) -> [u8; 32] {
        self.fragment
    }

    /// The canonical body's length in bytes.
    pub(crate) fn size(&self) -> u32 {
        self.size
    }

    /// What one ordinal of the body names.
    pub(crate) fn target(&self, ordinal: u32) -> Option<ir::frag::Target> {
        self.targets.get(ordinal as usize).copied()
    }

    fn ordinal(&self, target: ir::frag::Target) -> Option<u32> {
        self.by_target.get(&target).copied()
    }

    /// The site a recorded translator site becomes at rest. `None` when the body does not carry the
    /// reference the code baked, which would mean the body and the code disagree.
    fn site(&self, site: &Site, func: u32) -> Option<Target> {
        let callee = |id: ir::FuncId| -> Option<Ref> {
            match id == func {
                true => Some(Ref::Own),
                false => Some(Ref::Ordinal(self.ordinal(ir::frag::Target::Func(id))?)),
            }
        };
        Some(match site {
            Site::Frozen(addr) => Target::Link(self.ordinal(ir::frag::Target::Link(*addr))?),
            Site::Func(id) => match callee(*id)? {
                // The helpers take a func id itself, so `Own` is the id and not a PLT slot.
                Ref::Own => Target::OwnId,
                Ref::Ordinal(ordinal) => Target::FuncId(ordinal),
            },
            Site::Slot { domain, func } => Target::Slot {
                domain: *domain,
                func: callee(*func)?,
            },
            Site::Stub(id) => Target::Stub(self.ordinal(ir::frag::Target::Asm(*id))?),
            Site::Body(part) => Target::Body(*part),
        })
    }
}

/// One relocation the backend left in the code.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Reloc {
    pub offset: u32,
    pub kind: Kind,
    pub addend: i64,
    pub target: Target,
}

/// One symbol of a compiled function: its code, the absolutes in it, and how to unwind through it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Symbol {
    pub role: JitSymbolRole,
    /// The name the backend knows this symbol by (`f{func}`, `g{func}`, `p{func}`), which is how a
    /// relocation from another symbol of the same entry finds it again.
    pub name: Box<str>,
    pub code: Vec<u8>,
    pub relocs: Vec<Reloc>,
    /// The CFA program the loading session turns into this symbol's FDE at the address it lands at.
    /// The backend gives one for every defined symbol of a target with unwind info.
    pub unwind: Option<cranelift_codegen::isa::unwind::UnwindInfo>,
    /// The LSDA body of a symbol that can be unwound into, with its code-relative landing pads.
    pub lsda: Option<Vec<u8>>,
}

/// Everything that decides the code and is not the fragment: the jit-key of the cache family.
///
/// Two entries of one fragment are interchangeable exactly when this material is equal, so the store
/// keys by its digest and the loading session compares it field by field before it links anything.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct JitKey {
    /// The build that produced the entry: helper ABI, translator behaviour and TLS offsets are
    /// build-stable, so no finer versioning is needed and no cross-build reuse is promised.
    pub build_id: String,
    /// The target the code was built for.
    pub triple: String,
    /// The codegen options, one `name=value` per line: the optimization level is one of them.
    pub options: String,
    /// The ISA-dependent options, host CPU detection included, one `name=value` per line. Two hosts
    /// that enable different instructions must not share code.
    pub isa: String,
    /// The code domain: one namespace per vmctx regime, so a T→R flip cannot reuse the other's code.
    pub domain: CodeDomain,
}

impl JitKey {
    /// Take the key of the ISA one domain was built with.
    pub(crate) fn of(isa: &dyn cranelift_codegen::isa::TargetIsa, domain: CodeDomain) -> JitKey {
        let lines = |values: Vec<cranelift_codegen::settings::Value>| {
            values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        JitKey {
            build_id: crate::options::build::BUILD_ID.to_string(),
            triple: isa.triple().to_string(),
            options: lines(isa.flags().iter().collect()),
            isa: lines(isa.isa_flags()),
            domain,
        }
    }

    /// The digest the store keys entries by.
    pub(crate) fn digest(&self) -> [u8; 32] {
        *blake3::hash(&postcard::to_stdvec(self).unwrap_or_default()).as_bytes()
    }
}

/// Everything one compiled function publishes, in the order its symbols were defined.
///
/// The entry does not carry the function id it was compiled for: the id is the program's, the key is
/// the fragment's, and the session that links the entry back knows which function it is compiling.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Entry {
    /// The fragment the entry's code was compiled from: its semantic key, and the one thing the
    /// loading session has to find in the entry before binding anything.
    pub fragment: [u8; 32],
    /// What the code was built with, for the exact comparison a digest alone cannot give.
    pub jit: JitKey,
    pub symbols: Vec<Symbol>,
}

impl Entry {
    pub(crate) fn symbol(&self, role: JitSymbolRole) -> Option<&Symbol> {
        self.symbols.iter().find(|symbol| symbol.role == role)
    }

    /// The entry's bytes: one version byte, then the canonical form.
    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        let mut bytes = vec![VERSION];
        bytes.extend_from_slice(&postcard::to_stdvec(self).map_err(|e| e.to_string())?);
        Ok(bytes)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Entry, String> {
        match bytes.split_first() {
            Some((&VERSION, rest)) => {
                postcard::from_bytes(rest).map_err(|e| format!("entry decode failed: {e}"))
            }
            Some((&other, _)) => Err(format!("entry encoding version {other} is not {VERSION}")),
            None => Err("entry is empty".into()),
        }
    }
}

/// One symbol's capture input: what the backend produced for it, and the body's numbering.
pub(crate) struct Captured<'a> {
    pub module: &'a JITModule,
    pub func: u32,
    pub role: JitSymbolRole,
    pub name: &'a str,
    pub ir_func: &'a Function,
    pub compiled: &'a cranelift_codegen::CompiledCode,
    pub sites: &'a reloc::Sites,
    pub ordinals: &'a Ordinals,
    /// The CFA program the backend built for this symbol, and its LSDA when it has one.
    pub unwind: Option<cranelift_codegen::isa::unwind::UnwindInfo>,
    pub lsda: Option<Vec<u8>>,
}

/// Read one defined symbol back out of the backend: its code, and every relocation in it named in
/// canonical form. `None` when the code carries something this engine cannot replay — the entry is
/// then not storable rather than silently incomplete.
pub(crate) fn capture(input: Captured<'_>) -> Option<Symbol> {
    let Captured {
        module,
        func,
        role,
        name,
        ir_func,
        compiled,
        sites,
        ordinals,
        unwind,
        lsda,
    } = input;
    let user_names = ir_func.params.user_named_funcs();
    let mut relocs = Vec::new();
    for reloc in compiled.buffer.relocs() {
        let kind = match Kind::of(reloc.kind) {
            Some(kind) => kind,
            None => {
                if crate::options::jit_debug_dump() {
                    eprintln!(
                        "mirvm-jit-debug: {name} carries reloc {:?}, which this engine does not apply",
                        reloc.kind
                    );
                }
                return None;
            }
        };
        let target = match &reloc.target {
            FinalizedRelocTarget::ExternalName(ExternalName::User(reference)) => {
                let user = user_names.get(*reference)?;
                if user.namespace == reloc::DATA_NAMESPACE {
                    ordinals.site(&sites.at(user.index)?, func)?
                } else {
                    let declared = module
                        .declarations()
                        .get_functions()
                        .find(|(id, _)| id.as_u32() == user.index)
                        .and_then(|(_, declaration)| declaration.name.clone())?;
                    Target::Named(canonical_name(&declared))
                }
            }
            // A libcall or a named runtime symbol is not part of this engine's vocabulary.
            FinalizedRelocTarget::ExternalName(_) => return None,
            FinalizedRelocTarget::Func(offset) => Target::Local(*offset),
        };
        relocs.push(Reloc {
            offset: reloc.offset,
            kind,
            addend: reloc.addend,
            target,
        });
    }
    Some(Symbol {
        role,
        name: canonical_name(name),
        code: compiled.code_buffer().to_vec(),
        relocs,
        unwind,
        lsda,
    })
}

/// The name one of this compiler's own symbols carries in an entry.
///
/// The backend mints `f{func}`, `g{func}` and `p{func}` — the program's function id is in the name so
/// the module can hold several compiles at once — but no entry may depend on a program's numbering, so
/// the id is dropped here and the entry refers to its own symbols by role. A name that is not one of
/// those three is a helper of the import whitelist (or a trampoline, which no entry may name) and keeps
/// the name the whitelist gives it.
fn canonical_name(name: &str) -> Box<str> {
    let numbered = matches!(name.as_bytes().first(), Some(b'f' | b'g' | b'p'))
        && name.len() > 1
        && name[1..].bytes().all(|byte| byte.is_ascii_digit());
    match numbered {
        true => name[..1].into(),
        false => name.into(),
    }
}

/// One symbol as it landed in the linked region.
pub(crate) struct LinkedSymbol {
    pub role: JitSymbolRole,
    pub addr: u64,
    pub size: u64,
}

/// Code linked into a fresh executable region, owned by this value. The caller that wants
/// process-lifetime code leaks it; a test drops it and the mapping goes away.
pub(crate) struct Linked {
    base: *mut u8,
    len: usize,
    /// The linked symbols, in the order the roles were asked for.
    pub symbols: Vec<LinkedSymbol>,
}

impl Linked {
    pub(crate) fn entry(&self, role: JitSymbolRole) -> Option<u64> {
        self.symbols
            .iter()
            .find(|symbol| symbol.role == role)
            .map(|symbol| symbol.addr)
    }
}

impl Drop for Linked {
    fn drop(&mut self) {
        unsafe { crate::os::mem::unmap(self.base, self.len) };
    }
}

/// Link the requested symbols of one entry into one executable region.
///
/// `resolve` answers what a named symbol or a site is *now*: the import whitelist for a helper, the
/// live dispatch state for a site. A name that matches one of the linked symbols resolves inside the
/// entry, which is how the guarded wrapper finds the body it was built around. Every relocation must
/// resolve — an entry that cannot be linked whole is a miss, because a partially patched code region
/// would run with wrong values.
pub(crate) fn link(
    entry: &Entry,
    roles: &[JitSymbolRole],
    mut resolve: impl FnMut(&Target) -> Option<u64>,
) -> Result<Linked, String> {
    let wanted: Vec<&Symbol> = roles
        .iter()
        .filter_map(|role| entry.symbol(*role))
        .collect();
    if wanted.len() != roles.len() {
        return Err("entry does not hold every requested symbol".into());
    }
    // One region, symbols laid out in request order: an intra-entry reference is then a fixed offset
    // from the region's base, and the entry addresses all come from one mapping.
    let total: usize = wanted.iter().map(|symbol| symbol.code.len()).sum();
    if total == 0 {
        return Err("entry holds no code".into());
    }
    let page = crate::os::mem::page_size();
    let len = total.next_multiple_of(page);
    let base = crate::os::mem::map_anon(len, crate::os::mem::Prot::RW, false);
    if base.is_null() {
        return Err(format!("cannot map {len} bytes of code"));
    }
    let mut placed: Vec<(JitSymbolRole, &Symbol, u64)> = Vec::new();
    let mut offset = 0u64;
    for symbol in &wanted {
        let at = base as u64 + offset;
        unsafe {
            std::ptr::copy_nonoverlapping(
                symbol.code.as_ptr(),
                base.add(offset as usize),
                symbol.code.len(),
            );
        }
        placed.push((symbol.role, symbol, at));
        offset += symbol.code.len() as u64;
    }
    for (_, symbol, at) in &placed {
        for reloc in &symbol.relocs {
            let value = match &reloc.target {
                Target::Local(offset) => *at + u64::from(*offset),
                Target::Named(name) => placed
                    .iter()
                    .find(|(_, own, _)| own.name.as_ref() == name.as_ref())
                    .map(|(_, _, addr)| *addr)
                    .or_else(|| resolve(&reloc.target))
                    .ok_or_else(|| format!("unresolved symbol `{name}`"))?,
                // Everything else is a reference of the fragment, an interior of the body it belongs
                // to, or this entry's own id: the caller answers those from the body it links for.
                _ => {
                    resolve(&reloc.target).ok_or_else(|| format!("unresolved target {reloc:?}"))?
                }
            };
            let what = (value as i64).wrapping_add(reloc.addend) as u64;
            let at = *at + u64::from(reloc.offset);
            unsafe {
                match reloc.kind {
                    Kind::Abs32 => {
                        let value = u32::try_from(what)
                            .map_err(|_| format!("{what:#x} does not fit an Abs32 field"))?;
                        std::ptr::write_unaligned(at as *mut u32, value);
                    }
                    Kind::Abs64 => std::ptr::write_unaligned(at as *mut u64, what),
                    Kind::PcRel32 => {
                        let delta = (what as i64).wrapping_sub(at as i64);
                        let value = i32::try_from(delta)
                            .map_err(|_| format!("{delta} does not fit a PcRel32 field"))?;
                        std::ptr::write_unaligned(at as *mut i32, value);
                    }
                }
            }
        }
    }
    // W^X: the region is only executable once every value is in place.
    crate::os::mem::protect(base, len, crate::os::mem::Prot::RX)
        .map_err(|error| format!("cannot protect linked code: {error}"))?;
    Ok(Linked {
        base,
        len,
        symbols: placed
            .iter()
            .map(|(role, symbol, addr)| LinkedSymbol {
                role: *role,
                addr: *addr,
                size: symbol.code.len() as u64,
            })
            .collect(),
    })
}

/// What a linked target is *now*, in this process: the value the translator baked for the site the
/// target names.
///
/// The vocabulary is program-independent; the values are not. A stored entry carries the target, and
/// the link asks this function for the address to patch in, reading the fragment's own references back
/// out of the body's canonical ordinals — the same answers the translator baked when it first compiled
/// the body, one indirection later. `func` is the body the sites belong to: the resident decoded body
/// is what a `BodyRef` re-derives its interior from.
pub(crate) fn target_value(
    shared: &crate::vm::ctx::Shared,
    func: u32,
    ordinals: &Ordinals,
    target: &Target,
) -> Option<u64> {
    let callee = |reference: &Ref| -> Option<ir::FuncId> {
        match reference {
            Ref::Own => Some(func),
            Ref::Ordinal(ordinal) => match ordinals.target(*ordinal)? {
                ir::frag::Target::Func(id) => Some(id),
                _ => None,
            },
        }
    };
    match target {
        // The value the helpers take *is* the id.
        Target::OwnId => Some(u64::from(func)),
        Target::FuncId(ordinal) => match ordinals.target(*ordinal)? {
            ir::frag::Target::Func(id) => Some(u64::from(id)),
            _ => None,
        },
        Target::Slot { domain, func } => {
            let callee = callee(func)?;
            let slots = shared.jit.slots_for(*domain);
            slots
                .slots_fast
                .get(callee as usize)
                .map(|slot| slot as *const std::sync::atomic::AtomicU64 as u64)
        }
        Target::Stub(ordinal) => match ordinals.target(*ordinal)? {
            ir::frag::Target::Asm(id) => shared.instance.asm_stub_addrs.get(id as usize).copied(),
            _ => None,
        },
        // The body's own frozen addresses, through this Engine's load map: a link address is the
        // artifact's, and where the instance put that memory is the LoadMap's to say. The two coincide
        // for a fixed-base arena and differ for a dynamic one, which is exactly why the translator asks
        // the same question the same way.
        Target::Link(ordinal) => match ordinals.target(*ordinal)? {
            ir::frag::Target::Link(addr) => Some(shared.instance.resolve_link_addr(addr)),
            _ => None,
        },
        Target::Body(part) => body_value(shared, func, part),
        Target::Named(_) | Target::Local(_) => None,
    }
}

/// The address of one interior of the resident decoded body: the statement, rvalue, builtin,
/// signature, symbol bytes or trap message a slow-path helper re-matches.
fn body_value(shared: &crate::vm::ctx::Shared, func: u32, part: &Body) -> Option<u64> {
    use crate::vm::ir::{Stmt, Terminator};
    let body = shared.module.funcs.get(func as usize)?;
    let block = |block: &u32| body.blocks.get(*block as usize);
    match *part {
        Body::Stmt { block: b, item } => {
            let st = block(&b)?.stmts.get(item as usize)?;
            Some(st as *const Stmt as u64)
        }
        Body::Rvalue { block: b, item } => {
            let st = block(&b)?.stmts.get(item as usize)?;
            match st {
                Stmt::Assign { rv, .. } => Some(rv as *const _ as u64),
                _ => None,
            }
        }
        Body::Builtin { block: b } => match &block(&b)?.term {
            Terminator::CallBuiltin { builtin, .. } => Some(builtin as *const _ as u64),
            _ => None,
        },
        Body::ForeignSig { block: b, part } => match &block(&b)?.term {
            Terminator::CallForeign { sym, sig, .. } => Some(match part {
                SigPart::Signature => sig as *const _ as u64,
                SigPart::Symbol => sym.as_ptr() as u64,
            }),
            // An indirect call carries the signature of a native callee (a runtime `dlsym` result)
            // and no symbol: the same site, re-matched on the terminator the writer read it from.
            Terminator::CallIndirect {
                native_sig: Some(sig),
                ..
            } => Some(match part {
                SigPart::Signature => sig as *const _ as u64,
                SigPart::Symbol => return None,
            }),
            _ => None,
        },
        Body::TrapReason { block: b, item } => {
            let term = &block(&b)?.term;
            match item {
                Some(item) => match block(&b)?.stmts.get(item as usize)? {
                    Stmt::Trap(reason) => Some(reason.as_ptr() as u64),
                    _ => None,
                },
                None => match term {
                    Terminator::Trap(reason) => Some(reason.as_ptr() as u64),
                    _ => None,
                },
            }
        }
    }
}
