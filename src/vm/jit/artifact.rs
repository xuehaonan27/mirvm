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

use super::reloc::{self, Site};
use super::state::JitSymbolRole;
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
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Target {
    /// A symbol of the import whitelist, or another symbol of this entry.
    Named(Box<str>),
    /// A site of the recording vocabulary: the program's own frozen data, function ids, stub
    /// addresses, PLT slots and resident bodies.
    Site(Site),
    /// An offset inside this symbol's own code, for a reference the backend resolved to a label.
    Local(u32),
}

/// One relocation the backend left in the code.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Reloc {
    pub offset: u32,
    pub kind: Kind,
    pub addend: i64,
    pub target: Target,
}

/// One symbol of a compiled function: its code and the absolutes in it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Symbol {
    pub role: JitSymbolRole,
    /// The name the backend knows this symbol by (`f{func}`, `g{func}`, `p{func}`), which is how a
    /// relocation from another symbol of the same entry finds it again.
    pub name: Box<str>,
    pub code: Vec<u8>,
    pub relocs: Vec<Reloc>,
}

/// Everything one compiled function publishes, in the order its symbols were defined.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Entry {
    pub func: u32,
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

/// Read one defined symbol back out of the backend: its code, and every relocation in it named in
/// canonical form. `None` when the code carries something this engine cannot replay — the entry is
/// then not storable rather than silently incomplete.
pub(crate) fn capture(
    module: &JITModule,
    role: JitSymbolRole,
    name: &str,
    ir_func: &Function,
    compiled: &cranelift_codegen::CompiledCode,
    sites: &[Site],
    site_data: &[u32],
) -> Option<Symbol> {
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
                    let ordinal = site_data.iter().position(|id| *id == user.index)?;
                    Target::Site(sites[ordinal])
                } else {
                    let name = module
                        .declarations()
                        .get_functions()
                        .find(|(id, _)| id.as_u32() == user.index)
                        .and_then(|(_, declaration)| declaration.name.clone())?;
                    Target::Named(name.into())
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
        name: name.into(),
        code: compiled.code_buffer().to_vec(),
        relocs,
    })
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
                Target::Site(_) => {
                    resolve(&reloc.target).ok_or_else(|| format!("unresolved site {reloc:?}"))?
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

/// What a site names *now*, in this process.
///
/// The vocabulary is process-independent; the values are not. A stored entry carries the site, and
/// the link asks this function for the address to patch in, which is the same answer the translator
/// baked when it first compiled the body. `func` is the body the sites belong to: the resident
/// decoded body is what a `BodyRef` re-derives its interior from.
pub(crate) fn site_value(shared: &crate::vm::ctx::Shared, func: u32, site: &Site) -> Option<u64> {
    match site {
        // The body's own frozen addresses are this process's fixed addresses.
        Site::Frozen(addr) => Some(addr.0),
        // A function id immediate: the value the helpers take *is* the id.
        Site::Func(id) => Some(u64::from(*id)),
        Site::Stub(id) => shared.instance.asm_stub_addrs.get(*id as usize).copied(),
        Site::Slot { domain, func } => {
            let slots = shared.jit.slots_for(*domain);
            slots
                .slots_fast
                .get(*func as usize)
                .map(|slot| slot as *const std::sync::atomic::AtomicU64 as u64)
        }
        Site::Body(part) => body_interior(shared, func, part),
    }
}

/// The address of one interior of the resident decoded body: the statement, rvalue, builtin,
/// signature, symbol bytes or trap message a slow-path helper re-matches.
fn body_interior(shared: &crate::vm::ctx::Shared, func: u32, part: &Body) -> Option<u64> {
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
