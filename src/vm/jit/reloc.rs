//! The absolutes a compiled guest body names, as a closed vocabulary.
//!
//! Machine code may only bake an address this process can name again: a helper of the import
//! whitelist, a function id the program owns, a frozen-region address, an asm-stub entry, a PLT slot,
//! or an interior of the resident decoded body. Every one of them is recorded where it is baked, and
//! the record is what a stored entry replays once the code has been dropped.
//!
//! The *offset* comes from the backend rather than from a parallel bookkeeping pass: a site is emitted
//! as a named `global_value`, so cranelift records a relocation exactly where the immediate lands, the
//! name is resolved through the module's lookup hook to the value this process baked, and the compiler
//! reads the finalized code's relocation list back into the site table. A raw `iconst` of an address
//! leaves no relocation to find, which is why no translator site may use one.

use std::collections::HashMap;
use std::sync::Mutex;

use cranelift_codegen::ir::ExternalName;

use crate::vm::ir;

use super::CodeDomain;

/// One place a compiled body names something that exists only in this process.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Site {
    /// A frozen-region address: a static, a constant's bytes, a promoted allocation.
    Frozen(ir::LinkAddr),
    /// A function's absolute id in this program: the body itself, a callee, or a foreign function.
    Func(ir::FuncId),
    /// An asm-stub entry address.
    Stub(ir::AsmStubId),
    /// The PLT slot a call goes through: `slots_fast[func]` of `domain`.
    Slot {
        domain: CodeDomain,
        func: ir::FuncId,
    },
    /// A pointer into the resident decoded body (see [`Body`]).
    Body(Body),
}

/// Which interior of the resident decoded body a [`Site::Body`] names.
///
/// The loader re-derives the address from the body it demands before publishing the entry, so the
/// variant names the interior rather than an offset into an allocation that did not survive.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Body {
    /// The `item`-th statement of `block`, for the helpers that re-match it.
    Stmt { block: u32, item: u32 },
    /// The `item`-th statement's rvalue, for the wide helpers that re-match it.
    Rvalue { block: u32, item: u32 },
    /// The builtin a `block`'s terminator calls.
    Builtin { block: u32 },
    /// The foreign signature of a `block`'s call, or the symbol string inside it.
    ForeignSig { block: u32, part: SigPart },
    /// A trap message: a statement's reason (`item`) or a terminator's (`None`).
    TrapReason { block: u32, item: Option<u32> },
}

/// The two halves of a foreign call's signature data a helper may need.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum SigPart {
    /// The `ForeignSig` itself.
    Signature,
    /// The symbol string's bytes inside it.
    Symbol,
}

/// Where one site landed: its offset in the function's machine code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Placed {
    pub offset: u32,
    pub site: Site,
}

/// The values this compile has baked, by site name: the module resolves a site's symbol through here
/// at finalize, which is what makes the immediate correct without the translator knowing the
/// relocation's offset.
pub(crate) type Values = Mutex<HashMap<Box<str>, usize>>;

/// The symbol a site is emitted as. The name carries the function and the site's ordinal, which is
/// what maps the backend's relocation list back onto the recorded sites.
pub(crate) fn name(func: u32, ordinal: usize) -> String {
    format!("mirvm_site_{func}_{ordinal}")
}

/// The module's namespace for data objects (`cranelift-module` mints one for functions and one for
/// data; a relocation's target tells the two apart by this number).
pub(crate) const DATA_NAMESPACE: u32 = 1;

/// Where each recorded site landed, read from the compiled code's relocation list.
///
/// A site's symbol is a data declaration, so its relocation carries the module-level data id; that id
/// maps back onto the site's ordinal through the parallel `data` list the translator filled while
/// emitting. Anything else in the list (a helper call, a libcall) is not a site.
pub(crate) fn placed(
    func: &cranelift_codegen::ir::Function,
    compiled: &cranelift_codegen::CompiledCode,
    sites: &[Site],
    data: &[u32],
) -> Vec<Placed> {
    let names = func.params.user_named_funcs();
    let mut placed = Vec::new();
    for reloc in compiled.buffer.relocs() {
        let cranelift_codegen::FinalizedRelocTarget::ExternalName(ExternalName::User(reference)) =
            &reloc.target
        else {
            continue;
        };
        let Some(name) = names.get(*reference) else {
            continue;
        };
        if name.namespace != DATA_NAMESPACE {
            continue;
        }
        let Some(ordinal) = data.iter().position(|id| *id == name.index) else {
            continue;
        };
        placed.push(Placed {
            offset: reloc.offset,
            site: sites[ordinal],
        });
    }
    placed
}

/// Resolve one site name for the module's lookup hook. A name that is not a site (an import, a
/// libcall) is not ours to answer.
pub(crate) fn lookup(values: &Values, name: &str) -> Option<*const u8> {
    let address = *values.lock().ok()?.get(name)?;
    Some(address as *const u8)
}

/// Forget one function's names: the module caches what it resolved, so the value only has to live
/// until the function is finalized, and the table must not grow with the process's compile count.
pub(crate) fn forget(values: &Values, func: u32, count: usize) {
    let Ok(mut values) = values.lock() else {
        return;
    };
    for ordinal in 0..count {
        values.remove(name(func, ordinal).as_str());
    }
}
