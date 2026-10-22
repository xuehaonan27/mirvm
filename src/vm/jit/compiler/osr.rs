//! Session-local continuations borrowing an active interpreter frame.

use super::*;

#[cfg(test)]
mod tests;

impl<'a> Compiler<'a> {
    pub(super) fn for_osr(shared: &'a Shared, domain: CodeDomain) -> Self {
        let mut compiler = Self::with_tier(shared, domain, Tier::Optimized);
        compiler.osr = true;
        compiler.reload = false;
        compiler.jit_cache = false;
        compiler
    }

    pub(super) fn compile_osr(&mut self, func: u32) {
        let started = std::time::Instant::now();
        let slot = &self.shared.jit.slots_for(self.domain).osr[func as usize];
        if self.published.contains(&func) {
            return;
        }
        let Some((entry, ranges)) = self.define_osr(func) else {
            slot.store(FAIL_SENTINEL, Ordering::Release);
            return;
        };
        if crate::options::jit_debug() {
            let bytes: u64 = ranges.iter().map(|range| range.size).sum();
            crate::diag_debug!(
                Jit,
                "OSR f{func}: compiled {bytes} bytes in {} us",
                started.elapsed().as_micros()
            );
        }
        self.shared
            .jit
            .publish_osr(self.domain, func, entry, ranges);
        self.published.insert(func);
    }

    fn define_osr(&mut self, func: u32) -> Option<(u64, Vec<JitSymbolRange>)> {
        if self.domain == CodeDomain::Trace
            && self.shared.jit.trace_enter.load(Ordering::Acquire) == 0
        {
            return None;
        }
        let body = self.shared.module.funcs.get(func as usize)?;
        if !admit(self.shared, body)
            || !super::super::translate::backedge_targets(body).contains(&true)
        {
            return None;
        }
        let abi = CalleeAbi {
            nparams: 2,
            ..callee_abi(ir::FuncHeader::Owned(body))?
        };
        let (fast, fast_symbol) = self.define_fast(func, body, abi)?;
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::OsrBody) {
            return None;
        }
        let (packed, packed_symbol) = self.define_packed(func, body, abi, fast)?;
        let callees = self.prepare_callees(func, body)?;
        self.module.finalize_definitions().ok()?;
        reloc::forget(&self.values, func, self.sites.len());
        self.sites.clear();
        self.register_pending_eh_frames();
        self.publish_callees(callees);
        let ranges = self.finalized_symbol_ranges(vec![fast_symbol, packed_symbol]);
        Some((self.module.get_finalized_function(packed) as u64, ranges))
    }
}
