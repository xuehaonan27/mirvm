//! Frozen bytecode verification.
//!
//! Deserialization only proves that bytes have the right Rust shape. This pass
//! checks the indexes and memory ranges that the interpreter and JIT otherwise
//! consume with direct indexing or raw pointer arithmetic.

use super::ir::*;

const REGION_CAP: u64 = 1 << 30;

/// Numbering already occupied by lower image layers. Delta and dependency
/// images keep their final, absolute ids even while stored separately.
#[derive(Clone, Copy, Debug, Default)]
pub struct Prefix {
    pub funcs: usize,
    pub tls: usize,
    pub asm: usize,
}

pub fn module(module: &Module) -> Result<(), String> {
    module_with_prefix(module, Prefix::default())
}

pub fn module_with_prefix(module: &Module, prefix: Prefix) -> Result<(), String> {
    Verifier::new(module, prefix)?.run()
}

/// Indexed packages first verify module-level references; function bodies are decoded one by one
/// by the caller from mmap slices via `function_with_count`, so the entire function set is not kept in memory.
pub(crate) fn module_header_with_count(module: &Module, funcs: usize) -> Result<(), String> {
    Verifier::new_with_count(module, Prefix::default(), funcs)?.run_header()
}

pub(crate) fn function_with_count(
    module: &Module,
    funcs: usize,
    index: usize,
    body: &FuncBody,
) -> Result<(), String> {
    let verifier = Verifier::new_with_count(module, Prefix::default(), funcs)?;
    verifier
        .body(body)
        .map_err(|error| format!("function {index} `{}`: {error}", body.name))
}

pub(crate) fn main_role_counts(
    module: &Module,
    boundaries: usize,
    catchers: usize,
) -> Result<(), String> {
    if module.entry.is_some() && boundaries != 1 {
        return Err(format!(
            "executable module has {boundaries} main panic boundaries, expected exactly one"
        ));
    }
    if module.entry.is_some() && catchers != 1 {
        return Err(format!(
            "executable module has {catchers} main panic catchers, expected exactly one"
        ));
    }
    Ok(())
}

pub(crate) fn body_main_role_counts(body: &FuncBody) -> (usize, usize) {
    body.blocks
        .iter()
        .fold((0, 0), |(boundaries, catchers), block| match block.term {
            Terminator::Call {
                role: CallRole::MainPanicBoundary,
                ..
            } => (boundaries + 1, catchers),
            Terminator::CallBuiltin {
                role: BuiltinCallRole::MainPanicCatcher,
                ..
            } => (boundaries, catchers + 1),
            _ => (boundaries, catchers),
        })
}

struct Verifier<'a> {
    module: &'a Module,
    prefix: Prefix,
    funcs: usize,
    tls: usize,
    asm: usize,
}

impl<'a> Verifier<'a> {
    fn new(module: &'a Module, prefix: Prefix) -> Result<Self, String> {
        Self::new_with_count(module, prefix, module.funcs.len())
    }

    fn new_with_count(
        module: &'a Module,
        prefix: Prefix,
        local_funcs: usize,
    ) -> Result<Self, String> {
        let funcs = total("function", prefix.funcs, local_funcs)?;
        let tls = total("TLS", prefix.tls, module.tls.len())?;
        let asm = total("inline-asm stub", prefix.asm, module.asm_sites.len())?;
        Ok(Self {
            module,
            prefix,
            funcs,
            tls,
            asm,
        })
    }

    fn run(&self) -> Result<(), String> {
        self.run_header()?;
        let mut main_boundaries = 0;
        let mut main_catchers = 0;
        for (i, body) in self.module.funcs.iter().enumerate() {
            self.body(body)
                .map_err(|e| format!("function {} `{}`: {e}", self.prefix.funcs + i, body.name))?;
            let (boundaries, catchers) = body_main_role_counts(body);
            main_boundaries += boundaries;
            main_catchers += catchers;
        }
        // Delta images can refer to a boundary stored in an earlier image layer. The merged
        // executable (prefix zero) must contain exactly one before it can run or be packed.
        if self.prefix.funcs == 0 {
            main_role_counts(self.module, main_boundaries, main_catchers)?;
        }
        Ok(())
    }

    fn run_header(&self) -> Result<(), String> {
        if self.module.entry.is_some() && self.module.frozen.is_none() {
            return Err("executable module has an entry plan but no frozen memory for argv".into());
        }
        if !self.module.asm_stub_addrs.is_empty()
            && self.module.asm_stub_addrs.len() != self.module.asm_sites.len()
            && self.module.asm_stub_addrs.len() != self.asm
        {
            return Err(format!(
                "inline-asm address table has {} entries, expected {} local or {} merged entries",
                self.module.asm_stub_addrs.len(),
                self.module.asm_sites.len(),
                self.asm
            ));
        }

        for (name, &id) in &self.module.exports {
            self.func(id).map_err(|e| format!("export `{name}`: {e}"))?;
        }
        for (&addr, &id) in &self.module.fn_addrs {
            if addr == 0 {
                return Err("function address table contains a null address".into());
            }
            self.func(id)
                .map_err(|e| format!("function address {addr:#x}: {e}"))?;
        }
        for (&addr, &id) in &self.module.link_fn_addrs {
            if addr.0 == 0 {
                return Err("logical function address table contains a null address".into());
            }
            self.func(id)
                .map_err(|e| format!("logical function address {:#x}: {e}", addr.0))?;
        }
        for (i, slot) in self.module.tls.iter().enumerate() {
            if slot.align == 0 || !slot.align.is_power_of_two() {
                return Err(format!(
                    "TLS slot {} has invalid alignment {}",
                    self.prefix.tls + i,
                    slot.align
                ));
            }
            let template = self.module.try_resolve_link_addr(slot.template)?;
            self.frozen_range(template, slot.size, false)
                .map_err(|e| format!("TLS slot {} template: {e}", self.prefix.tls + i))?;
        }
        for (i, fixup) in self.module.got_fixups.iter().enumerate() {
            if fixup.sym as usize >= self.module.foreign_syms.len() {
                return Err(format!(
                    "GOT fixup {i} refers to symbol {}, but only {} symbols exist",
                    fixup.sym,
                    self.module.foreign_syms.len()
                ));
            }
            let addr = self.module.try_resolve_link_addr(fixup.addr)?;
            self.frozen_range(addr, 8, false)
                .map_err(|e| format!("GOT fixup {i}: {e}"))?;
        }
        for (i, reloc) in self.module.frozen_relocs.iter().enumerate() {
            let at = self.module.try_resolve_link_addr(reloc.at)?;
            self.frozen_range(at, 8, false)
                .map_err(|e| format!("frozen relocation {i} write address: {e}"))?;
            match reloc.target {
                FrozenRelocTarget::Frozen(target) => {
                    let target = self.module.try_resolve_link_addr(target)?;
                    self.frozen_range(target, 0, true)
                        .map_err(|e| format!("frozen relocation {i} target: {e}"))?;
                }
                FrozenRelocTarget::Entry(target) => {
                    if !self.module.link_fn_addrs.contains_key(&target) {
                        return Err(format!(
                            "frozen relocation {i} refers to unknown entry {:#x}",
                            target.0
                        ));
                    }
                }
            }
        }
        let mut entry_sites = std::collections::HashMap::new();
        {
            let mut verify_entry_site = |label: &str, site: &EntryStubSite| -> Result<(), String> {
                self.func(site.func).map_err(|e| format!("{label}: {e}"))?;
                if self.module.link_fn_addrs.get(&site.link_addr) != Some(&site.func) {
                    return Err(format!(
                        "{label} link address {:#x} is absent or names a different function",
                        site.link_addr.0
                    ));
                }
                if self.module.load_map.resolves_frozen(site.link_addr) {
                    return Err(format!(
                        "{label} link address {:#x} overlaps frozen memory",
                        site.link_addr.0
                    ));
                }
                if entry_sites.insert(site.link_addr, site.func).is_some() {
                    return Err(format!(
                        "{label} duplicates entry link address {:#x}",
                        site.link_addr.0
                    ));
                }
                self.foreign_sig(&site.sig)
                    .map_err(|e| format!("{label}: {e}"))?;
                Ok(())
            };
            for (i, site) in self.module.entry_stub_sites.iter().enumerate() {
                verify_entry_site(&format!("entry stub {i}"), site)?;
            }
            for (arena_i, (_, sites, _)) in self.module.image_entry_stubs.iter().enumerate() {
                for (site_i, site) in sites.iter().enumerate() {
                    verify_entry_site(&format!("image entry stub {arena_i}:{site_i}"), site)?;
                }
            }
        }
        if self.module.load_map.is_strict() {
            for (&addr, &func) in &self.module.link_fn_addrs {
                if !self.module.load_map.resolves_frozen(addr)
                    && entry_sites.get(&addr) != Some(&func)
                {
                    return Err(format!(
                        "logical function address {:#x} is outside frozen memory but has no matching entry stub",
                        addr.0
                    ));
                }
            }
        }
        if let Some(shims) = self.module.custom_alloc_shims {
            for (name, id) in [
                ("alloc", shims.alloc),
                ("dealloc", shims.dealloc),
                ("realloc", shims.realloc),
                ("alloc_zeroed", shims.alloc_zeroed),
            ] {
                self.func(id)
                    .map_err(|e| format!("global allocator `{name}` shim: {e}"))?;
            }
        }
        if let Some(plan) = self.module.guest_panic_cleanup {
            if plan.cleanup == plan.drop_payload {
                return Err(
                    "guest panic cleanup and payload drop glue refer to the same function".into(),
                );
            }
            self.func(plan.cleanup)
                .map_err(|e| format!("guest panic cleanup: {e}"))?;
            self.func(plan.drop_payload)
                .map_err(|e| format!("guest panic payload drop glue: {e}"))?;
        }
        if let Some(entry) = self.module.entry {
            self.func(entry.lang_start)
                .map_err(|e| format!("entry lang_start: {e}"))?;
            let known = if self.module.link_fn_addrs.is_empty() {
                self.module.fn_addrs.contains_key(&entry.main_addr.0)
            } else {
                self.module.link_fn_addrs.contains_key(&entry.main_addr)
            };
            if !known {
                return Err(format!(
                    "entry main address {:#x} is absent from the function address table",
                    entry.main_addr
                ));
            }
        }

        Ok(())
    }

    fn body(&self, body: &FuncBody) -> Result<(), String> {
        if body.frame_align == 0 || !body.frame_align.is_power_of_two() {
            return Err(format!("invalid frame alignment {}", body.frame_align));
        }
        if u64::from(body.frame_size) > REGION_CAP || u64::from(body.frame_align) > REGION_CAP {
            return Err(format!(
                "frame size/alignment ({}/{}) exceeds the 1 GiB operand region",
                body.frame_size, body.frame_align
            ));
        }
        if body.blocks.is_empty() {
            return Err("has no basic blocks".into());
        }
        self.ret_abi(body, &body.ret)?;
        for (i, param) in body.params.iter().enumerate() {
            self.param_abi(body, param)
                .map_err(|e| format!("parameter {i}: {e}"))?;
        }
        if let Some(off) = body.caller_loc_off {
            self.span(body, off, 8, "track_caller slot")?;
        }
        for (bb, block) in body.blocks.iter().enumerate() {
            for (si, stmt) in block.stmts.iter().enumerate() {
                self.stmt(body, stmt)
                    .map_err(|e| format!("bb{bb} statement {si}: {e}"))?;
            }
            self.term(body, &block.term)
                .map_err(|e| format!("bb{bb} terminator: {e}"))?;
        }
        Ok(())
    }

    fn param_abi(&self, body: &FuncBody, abi: &ParamAbi) -> Result<(), String> {
        match abi {
            ParamAbi::Zst => Ok(()),
            ParamAbi::Scalar(slot) => self.slot(body, *slot),
            ParamAbi::Pair(a, b) => {
                self.slot(body, *a)?;
                self.slot(body, *b)
            }
            ParamAbi::Indirect { off, size } => self.span(body, *off, *size, "indirect parameter"),
        }
    }

    fn ret_abi(&self, body: &FuncBody, abi: &RetAbi) -> Result<(), String> {
        match abi {
            RetAbi::Zst => Ok(()),
            RetAbi::Scalar(slot) => self.slot(body, *slot),
            RetAbi::Pair(a, b) => {
                self.slot(body, *a)?;
                self.slot(body, *b)
            }
            RetAbi::Indirect {
                ret_off,
                size,
                sret_off,
            } => {
                self.span(body, *ret_off, *size, "indirect return value")?;
                self.span(body, *sret_off, 8, "indirect return pointer")
            }
        }
    }

    fn stmt(&self, body: &FuncBody, stmt: &Stmt) -> Result<(), String> {
        match stmt {
            Stmt::Assign { dst, rv } => {
                self.scalar_place(body, dst)?;
                self.rvalue(body, rv)
            }
            Stmt::AssignOverflow {
                a,
                b,
                dst_val,
                dst_flag,
                ..
            } => {
                self.operands(body, [a, b])?;
                self.scalar_places(body, [dst_val, dst_flag])
            }
            Stmt::Copy { dst, src, .. } => self.places(body, [dst, src]),
            Stmt::RepeatScalar { dst, val, .. } => {
                self.place(body, dst)?;
                self.operand(body, val)
            }
            Stmt::AtomicStore { addr, val, .. } => self.operands(body, [addr, val]),
            Stmt::VolatileLoad { addr, dst, .. } => {
                self.operand(body, addr)?;
                self.place(body, dst)
            }
            Stmt::VolatileStore { addr, src, .. } => {
                self.operand(body, addr)?;
                self.place(body, src)
            }
            Stmt::AtomicCxchg {
                addr,
                expected,
                new,
                dst_val,
                dst_ok,
                ..
            } => {
                self.operands(body, [addr, expected, new])?;
                self.scalar_places(body, [dst_val, dst_ok])
            }
            Stmt::AtomicRmw { addr, val, dst, .. } => {
                self.operands(body, [addr, val])?;
                self.scalar_place(body, dst)
            }
            Stmt::MemCopy {
                dst, src, count, ..
            } => self.operands(body, [dst, src, count]),
            Stmt::MemSet {
                dst, val, count, ..
            } => self.operands(body, [dst, val, count]),
            Stmt::SimdBin {
                dst,
                a,
                b,
                lanes,
                lane_bytes,
                ..
            } => {
                vector(*lanes, *lane_bytes)?;
                self.places(body, [dst, a, b])
            }
            Stmt::SimdUn {
                dst,
                a,
                lanes,
                lane_bytes,
                ..
            } => {
                vector(*lanes, *lane_bytes)?;
                self.places(body, [dst, a])
            }
            Stmt::SimdFma {
                dst,
                a,
                b,
                c,
                lanes,
                lane_bytes,
            }
            | Stmt::SimdFunnel {
                dst,
                a,
                b,
                shift: c,
                lanes,
                lane_bytes,
                ..
            } => {
                vector(*lanes, *lane_bytes)?;
                self.places(body, [dst, a, b, c])
            }
            Stmt::SimdCast {
                dst,
                src,
                lanes,
                src_bytes,
                dst_bytes,
                ..
            } => {
                vector(*lanes, *src_bytes)?;
                vector(*lanes, *dst_bytes)?;
                self.places(body, [dst, src])
            }
            Stmt::SimdSelect {
                mask,
                a,
                b,
                dst,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.places(body, [mask, a, b, dst])
            }
            Stmt::SimdSelectBitmask {
                mask,
                a,
                b,
                dst,
                lanes,
                lane_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                self.operand(body, mask)?;
                self.places(body, [a, b, dst])
            }
            Stmt::SimdGather {
                passthru,
                ptrs,
                mask,
                dst,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.places(body, [passthru, ptrs, mask, dst])
            }
            Stmt::SimdScatter {
                values,
                ptrs,
                mask,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.places(body, [values, ptrs, mask])
            }
            Stmt::SimdMaskedLoad {
                mask,
                base,
                passthru,
                dst,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.operand(body, base)?;
                self.places(body, [mask, passthru, dst])
            }
            Stmt::SimdMaskedStore {
                mask,
                base,
                values,
                lanes,
                lane_bytes,
                mask_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                vector(*lanes, *mask_bytes)?;
                self.operand(body, base)?;
                self.places(body, [mask, values])
            }
            Stmt::SimdExtractDyn {
                src,
                idx,
                dst,
                lanes,
                lane_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                self.place(body, src)?;
                self.operand(body, idx)?;
                self.scalar_place(body, dst)
            }
            Stmt::SimdInsertDyn {
                src,
                idx,
                val,
                dst,
                lanes,
                lane_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                self.places(body, [src, dst])?;
                self.operands(body, [idx, val])
            }
            Stmt::SimdArithOffset {
                ptrs,
                offsets,
                dst,
                lanes,
                ..
            } => {
                if *lanes == 0 {
                    return Err("SIMD lane count is zero".into());
                }
                self.places(body, [ptrs, offsets, dst])
            }
            Stmt::SimdSplat {
                dst,
                val,
                lanes,
                lane_bytes,
            } => {
                vector(*lanes, *lane_bytes)?;
                self.place(body, dst)?;
                self.operand(body, val)
            }
            Stmt::Bin128 { a, b, dst, .. } => {
                self.places(body, [a, dst])?;
                self.bin128_rhs(body, b)
            }
            Stmt::Sat128 { a, b, dst, .. }
            | Stmt::F128Bin { a, b, dst, .. }
            | Stmt::F128MathBin {
                a,
                b: F128Rhs::Wide(b),
                dst,
                ..
            } => self.places(body, [a, b, dst]),
            Stmt::F128MathBin {
                a,
                b: F128Rhs::Scalar(b),
                dst,
                ..
            } => {
                self.places(body, [a, dst])?;
                self.operand(body, b)
            }
            Stmt::Wide128ToFloat { src, dst, .. }
            | Stmt::Bit128Count { src, dst, .. }
            | Stmt::F128ToScalar { src, dst, .. } => {
                self.place(body, src)?;
                self.scalar_place(body, dst)
            }
            Stmt::FloatToWide128 { src, dst, .. } | Stmt::F128FromScalar { src, dst, .. } => {
                self.operand(body, src)?;
                self.place(body, dst)
            }
            Stmt::Bit128 { src, dst, .. }
            | Stmt::F128Un { a: src, dst, .. }
            | Stmt::F128FromWideInt { src, dst, .. }
            | Stmt::F128ToWideInt { src, dst, .. } => self.places(body, [src, dst]),
            Stmt::F128Fma { a, b, c, dst } => self.places(body, [a, b, c, dst]),
            Stmt::NicheDiscr128 { tag, dst, .. } => {
                self.place(body, tag)?;
                self.scalar_place(body, dst)
            }
            Stmt::RepeatBytes { first, .. } => self.place(body, first),
            Stmt::Trap(_) | Stmt::Nop | Stmt::Fence { .. } => Ok(()),
        }
    }

    fn rvalue(&self, body: &FuncBody, rv: &Rvalue) -> Result<(), String> {
        match rv {
            Rvalue::Use(a)
            | Rvalue::NotBits(a)
            | Rvalue::NotBool(a)
            | Rvalue::Neg(a)
            | Rvalue::Cast { a, .. }
            | Rvalue::MathUn { a, .. }
            | Rvalue::FloatNeg { a, .. }
            | Rvalue::FloatCast { a, .. }
            | Rvalue::FloatToInt { a, .. }
            | Rvalue::IntToFloat { a, .. }
            | Rvalue::BitUn { a, .. }
            | Rvalue::AtomicLoad { addr: a, .. } => self.operand(body, a),
            Rvalue::TlsRef(id) => self.tls(*id),
            Rvalue::IntBin { a, b, .. }
            | Rvalue::IntCmp { a, b, .. }
            | Rvalue::PtrOffset {
                ptr: a, count: b, ..
            }
            | Rvalue::IntCmp3 { a, b, .. }
            | Rvalue::FloatBin { a, b, .. }
            | Rvalue::MathBin { a, b, .. }
            | Rvalue::UMax { a, b }
            | Rvalue::FloatCmp { a, b, .. }
            | Rvalue::PtrDiff { a, b, .. }
            | Rvalue::IntSat { a, b, .. } => self.operands(body, [a, b]),
            Rvalue::NicheDiscr { tag, .. } => self.operand(body, tag),
            Rvalue::MathFma { a, b, c, .. } | Rvalue::MemCmp { a, b, n: c } => {
                self.operands(body, [a, b, c])
            }
            Rvalue::Ref(p) => self.place(body, p),
            Rvalue::F128Cmp { a, b, .. } | Rvalue::Cmp128 { a, b, .. } => self.places(body, [a, b]),
            Rvalue::SimdBitmask {
                a,
                lanes,
                lane_bytes,
            }
            | Rvalue::SimdReduce {
                a,
                lanes,
                lane_bytes,
                ..
            }
            | Rvalue::SimdReduceArith {
                a,
                lanes,
                lane_bytes,
                ..
            } => {
                vector(*lanes, *lane_bytes)?;
                self.place(body, a)
            }
        }
    }

    fn term(&self, body: &FuncBody, term: &Terminator) -> Result<(), String> {
        match term {
            Terminator::Goto(bb) => self.bb(body, *bb),
            Terminator::SwitchInt {
                discr,
                targets,
                otherwise,
            } => {
                self.switch_discr(body, discr)?;
                for (_, bb) in targets {
                    self.bb(body, *bb)?;
                }
                self.bb(body, *otherwise)
            }
            Terminator::Call {
                callee,
                args,
                ret,
                target,
                unwind,
                role,
            } => {
                self.func(*callee)?;
                self.operands(body, args)?;
                self.ret_dest(body, ret)?;
                self.bb(body, *target)?;
                self.unwind(body, *unwind)?;
                if matches!(role, crate::vm::ir::CallRole::MainPanicBoundary)
                    && !matches!(unwind, UnwindAction::Continue)
                {
                    return Err("main panic boundary call must use Continue unwind action".into());
                }
                Ok(())
            }
            Terminator::CallBuiltin {
                builtin,
                args,
                ret,
                target,
                unwind,
                role,
            } => {
                self.operands(body, args)?;
                self.ret_dest(body, ret)?;
                self.bb(body, *target)?;
                self.unwind(body, *unwind)?;
                if matches!(role, BuiltinCallRole::MainPanicCatcher) {
                    let byte_ret = matches!(
                        ret,
                        RetDest::Scalar(ScalarPlace::Slot(Slot {
                            width: Width::W8,
                            ..
                        })) | RetDest::Scalar(ScalarPlace::Mem {
                            width: Width::W8,
                            ..
                        })
                    );
                    if !matches!(builtin, Builtin::CatchUnwind)
                        || !matches!(unwind, UnwindAction::Continue)
                        || args.len() != 3
                        || !args.iter().all(|arg| arg.width() == Width::W64)
                        || !byte_ret
                    {
                        return Err(
                            "main panic catcher must be CatchUnwind with three pointer-width \
                             arguments, a byte scalar return, and Continue unwind action"
                                .into(),
                        );
                    }
                }
                Ok(())
            }
            Terminator::CallForeign {
                sig,
                args,
                ret,
                target,
                unwind,
                ..
            } => {
                self.foreign_sig(sig)?;
                self.operands(body, args)?;
                self.ret_dest(body, ret)?;
                self.bb(body, *target)?;
                self.unwind(body, *unwind)
            }
            Terminator::CallIndirect {
                callee,
                args,
                ret,
                target,
                unwind,
                native_sig,
                ..
            } => {
                self.operand(body, callee)?;
                self.operands(body, args)?;
                self.ret_dest(body, ret)?;
                if let Some(sig) = native_sig {
                    self.foreign_sig(sig)?;
                }
                self.bb(body, *target)?;
                self.unwind(body, *unwind)
            }
            Terminator::InlineAsm {
                stub,
                buf_size,
                ins,
                outs,
                target,
            } => {
                self.asm(*stub)?;
                for (i, (off, val)) in ins.iter().enumerate() {
                    let width = match val {
                        AsmIoVal::Scalar(v) => {
                            self.operand(body, v)?;
                            8
                        }
                        AsmIoVal::VecBytes(p, n) => {
                            self.place(body, p)?;
                            *n
                        }
                    };
                    buffer_span(*buf_size, *off, width).map_err(|e| format!("input {i}: {e}"))?;
                }
                for (i, (off, dst)) in outs.iter().enumerate() {
                    let width = match dst {
                        AsmIoDst::Scalar(p) => {
                            self.scalar_place(body, p)?;
                            8
                        }
                        AsmIoDst::VecBytes(p, n) => {
                            self.place(body, p)?;
                            *n
                        }
                    };
                    buffer_span(*buf_size, *off, width).map_err(|e| format!("output {i}: {e}"))?;
                }
                self.bb(body, *target)
            }
            Terminator::Return
            | Terminator::Unreachable
            | Terminator::Resume
            | Terminator::TerminateAbort
            | Terminator::Trap(_) => Ok(()),
        }
    }

    fn foreign_sig(&self, sig: &ForeignSig) -> Result<(), String> {
        if sig.fixed.is_some_and(|n| n > sig.args.len()) {
            return Err(format!(
                "variadic fixed count exceeds argument count {}",
                sig.args.len()
            ));
        }
        for (i, kind) in sig.args.iter().enumerate() {
            if matches!(kind, FfiKind::Void) {
                return Err(format!("argument {i} has void type"));
            }
            self.ffi_kind(kind)
                .map_err(|e| format!("argument {i}: {e}"))?;
        }
        self.ffi_kind(&sig.ret)
            .map_err(|e| format!("return type: {e}"))?;
        for (i, (arg, nested)) in sig.thunk_args.iter().enumerate() {
            if *arg >= sig.args.len() {
                return Err(format!(
                    "callback entry {i} refers to missing argument {arg}"
                ));
            }
            if !matches!(sig.args[*arg], FfiKind::Ptr) {
                return Err(format!("callback argument {arg} is not a pointer"));
            }
            if !nested.thunk_args.is_empty() {
                return Err(format!("callback argument {arg} contains nested callbacks"));
            }
            self.foreign_sig(nested)
                .map_err(|e| format!("callback argument {arg}: {e}"))?;
        }
        Ok(())
    }

    fn ffi_kind(&self, kind: &FfiKind) -> Result<(), String> {
        match kind {
            FfiKind::Agg(agg) => ffi_agg(agg).map(|_| ()),
            FfiKind::I8
            | FfiKind::I16
            | FfiKind::I32
            | FfiKind::I64
            | FfiKind::U8
            | FfiKind::U16
            | FfiKind::U32
            | FfiKind::U64
            | FfiKind::F32
            | FfiKind::F64
            | FfiKind::Ptr
            | FfiKind::Void => Ok(()),
        }
    }

    fn operand(&self, body: &FuncBody, op: &Operand) -> Result<(), String> {
        match op {
            Operand::Slot(slot) => self.slot(body, *slot),
            Operand::Mem { expr, .. } | Operand::AddrOf(expr) => self.place(body, expr),
            Operand::Imm { .. } => Ok(()),
            Operand::AddrImm(addr) => match self.module.try_resolve_link_addr(*addr) {
                Ok(runtime) => self.frozen_range(runtime, 0, true),
                Err(_) if self.module.link_fn_addrs.contains_key(addr) => Ok(()),
                Err(error) => Err(error),
            },
            Operand::SubImm { base, .. } => self.operand(body, base),
        }
    }

    fn place(&self, body: &FuncBody, place: &PlaceExpr) -> Result<(), String> {
        match place.base {
            PlaceBase::Local(off) => {
                if off > body.frame_size {
                    return Err(format!(
                        "local offset {off} exceeds frame size {}",
                        body.frame_size
                    ));
                }
            }
            PlaceBase::Static(addr) => {
                let runtime = self.module.try_resolve_link_addr(addr)?;
                self.frozen_range(
                    runtime,
                    0,
                    self.prefix.funcs != 0 || self.prefix.tls != 0 || self.prefix.asm != 0,
                )?
            }
        }
        for step in &place.steps {
            match step {
                PlaceStep::Deref | PlaceStep::Offset(_) => {}
                PlaceStep::VTableAlignOffset { meta, packed, .. } => {
                    self.operand(body, meta)?;
                    if packed.is_some_and(|n| n == 0 || !n.is_power_of_two()) {
                        return Err("vtable packed alignment is not a non-zero power of two".into());
                    }
                }
                PlaceStep::IndexScaled { idx, .. } => self.slot(body, *idx)?,
            }
        }
        Ok(())
    }

    fn scalar_place(&self, body: &FuncBody, place: &ScalarPlace) -> Result<(), String> {
        match place {
            ScalarPlace::Slot(slot) => self.slot(body, *slot),
            ScalarPlace::Mem { expr, .. } => self.place(body, expr),
        }
    }

    fn ret_dest(&self, body: &FuncBody, ret: &RetDest) -> Result<(), String> {
        match ret {
            RetDest::Ignore => Ok(()),
            RetDest::Scalar(p) => self.scalar_place(body, p),
            RetDest::Pair(a, b) => self.scalar_places(body, [a, b]),
            RetDest::Indirect(p) => self.place(body, p),
        }
    }

    fn switch_discr(&self, body: &FuncBody, discr: &SwitchDiscr) -> Result<(), String> {
        match discr {
            SwitchDiscr::Scalar(op) => self.operand(body, op),
            SwitchDiscr::Wide(place) => self.place(body, place),
        }
    }

    fn bin128_rhs(&self, body: &FuncBody, rhs: &Bin128Rhs) -> Result<(), String> {
        match rhs {
            Bin128Rhs::Wide(p) => self.place(body, p),
            Bin128Rhs::Scalar(op) => self.operand(body, op),
        }
    }

    fn operands<'b>(
        &self,
        body: &FuncBody,
        ops: impl IntoIterator<Item = &'b Operand>,
    ) -> Result<(), String> {
        for op in ops {
            self.operand(body, op)?;
        }
        Ok(())
    }

    fn places<'b>(
        &self,
        body: &FuncBody,
        places: impl IntoIterator<Item = &'b PlaceExpr>,
    ) -> Result<(), String> {
        for place in places {
            self.place(body, place)?;
        }
        Ok(())
    }

    fn scalar_places<'b>(
        &self,
        body: &FuncBody,
        places: impl IntoIterator<Item = &'b ScalarPlace>,
    ) -> Result<(), String> {
        for place in places {
            self.scalar_place(body, place)?;
        }
        Ok(())
    }

    fn slot(&self, body: &FuncBody, slot: Slot) -> Result<(), String> {
        self.span(body, slot.off, slot.width.bytes(), "scalar slot")
    }

    fn span(&self, body: &FuncBody, off: u32, size: u32, what: &str) -> Result<(), String> {
        let end = off
            .checked_add(size)
            .ok_or_else(|| format!("{what} range overflows"))?;
        if end > body.frame_size {
            return Err(format!(
                "{what} {off}..{end} exceeds frame size {}",
                body.frame_size
            ));
        }
        Ok(())
    }

    fn func(&self, id: FuncId) -> Result<(), String> {
        if id as usize >= self.funcs {
            return Err(format!("function id {id} is outside 0..{}", self.funcs));
        }
        Ok(())
    }

    fn tls(&self, id: TlsId) -> Result<(), String> {
        if id as usize >= self.tls {
            return Err(format!("TLS id {id} is outside 0..{}", self.tls));
        }
        Ok(())
    }

    fn asm(&self, id: AsmStubId) -> Result<(), String> {
        if id as usize >= self.asm {
            return Err(format!(
                "inline-asm stub id {id} is outside 0..{}",
                self.asm
            ));
        }
        Ok(())
    }

    fn bb(&self, body: &FuncBody, bb: Bb) -> Result<(), String> {
        if bb as usize >= body.blocks.len() {
            return Err(format!(
                "basic block bb{bb} is outside 0..{}",
                body.blocks.len()
            ));
        }
        Ok(())
    }

    fn unwind(&self, body: &FuncBody, unwind: UnwindAction) -> Result<(), String> {
        match unwind {
            UnwindAction::Cleanup(bb) => self.bb(body, bb),
            UnwindAction::Continue | UnwindAction::Terminate => Ok(()),
        }
    }

    fn frozen_range(&self, addr: u64, size: u64, allow_prefix: bool) -> Result<(), String> {
        if addr == 0 {
            return Err("contains a null frozen address".into());
        }
        let end = addr
            .checked_add(size)
            .ok_or("frozen address range overflows")?;
        let contains = self
            .module
            .frozen
            .iter()
            .chain(self.module.image_frozens.iter())
            .any(|arena| {
                let bytes = arena.snapshot();
                let start = bytes.as_ptr() as u64;
                let limit = start + bytes.len() as u64;
                addr >= start && end <= limit
            });
        if !contains && !allow_prefix {
            return Err(format!(
                "address range {addr:#x}..{end:#x} is outside frozen memory"
            ));
        }
        Ok(())
    }
}

fn total(kind: &str, prefix: usize, local: usize) -> Result<usize, String> {
    let n = prefix
        .checked_add(local)
        .ok_or_else(|| format!("{kind} count overflows"))?;
    if n > u32::MAX as usize + 1 {
        return Err(format!("{kind} count {n} exceeds the IR id space"));
    }
    Ok(n)
}

fn vector(lanes: u16, lane_bytes: u8) -> Result<(), String> {
    if lanes == 0 || lane_bytes == 0 {
        return Err(format!(
            "invalid SIMD geometry: {lanes} lanes x {lane_bytes} bytes"
        ));
    }
    Ok(())
}

fn buffer_span(buf_size: u32, off: u32, width: u32) -> Result<(), String> {
    let end = off
        .checked_add(width)
        .ok_or("inline-asm buffer range overflows")?;
    if end > buf_size {
        return Err(format!("buffer range {off}..{end} exceeds size {buf_size}"));
    }
    Ok(())
}

fn ffi_agg(agg: &FfiAgg) -> Result<(u32, u32), String> {
    if agg.align == 0 || agg.align > 8 || !agg.align.is_power_of_two() {
        return Err(format!("aggregate has invalid alignment {}", agg.align));
    }
    let mut off = 0u32;
    let mut align = 1u32;
    for (i, field) in agg.fields.iter().enumerate() {
        let (size, field_align) = match &field.leaf {
            FfiLeaf::Scalar(kind) => ffi_scalar_layout(kind)
                .ok_or_else(|| format!("aggregate field {i} is not a scalar FFI value"))?,
            FfiLeaf::Agg(inner) => {
                ffi_agg(inner).map_err(|e| format!("aggregate field {i}: {e}"))?
            }
        };
        let expected = off
            .checked_add(field_align - 1)
            .map(|n| n & !(field_align - 1))
            .ok_or_else(|| format!("aggregate field {i} alignment overflows"))?;
        if field.off != expected {
            return Err(format!(
                "aggregate field {i} offset {} is not natural offset {expected}",
                field.off
            ));
        }
        off = field
            .off
            .checked_add(size)
            .ok_or_else(|| format!("aggregate field {i} range overflows"))?;
        align = align.max(field_align);
    }
    let size = off
        .checked_add(align - 1)
        .map(|n| n & !(align - 1))
        .ok_or("aggregate size overflows")?;
    if (size, align) != (agg.size, agg.align) {
        return Err(format!(
            "aggregate layout computes as size/alignment {size}/{align}, encoded as {}/{}",
            agg.size, agg.align
        ));
    }
    Ok((size, align))
}

fn ffi_scalar_layout(kind: &FfiKind) -> Option<(u32, u32)> {
    let n = match kind {
        FfiKind::I8 | FfiKind::U8 => 1,
        FfiKind::I16 | FfiKind::U16 => 2,
        FfiKind::I32 | FfiKind::U32 | FfiKind::F32 => 4,
        FfiKind::I64 | FfiKind::U64 | FfiKind::F64 | FfiKind::Ptr => 8,
        FfiKind::Void | FfiKind::Agg(_) => return None,
    };
    Some((n, n))
}

#[cfg(test)]
mod tests;
