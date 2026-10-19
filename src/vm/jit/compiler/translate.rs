//! One body at a time: the entry signatures, the fast/packed definitions, the c2i
//! trampoline a not-yet-compiled callee lands in, and the shape checks admission already made
//! again at the boundary.

use super::*;

impl<'a> Compiler<'a> {
    /// Whether this compiler's domain pins the context register: plain code where the pair can
    /// carry a pinned `Ctx` across compiled frames
    /// ([`crate::os_arch::PLAIN_CODE_PINS_CTX`]), never trace code (its register is the recorder,
    /// which that domain's own trampoline manages).
    fn plain_pins_ctx(&self) -> bool {
        self.domain == CodeDomain::Plain && crate::os_arch::PLAIN_CODE_PINS_CTX
    }

    /// Capture one wrapper symbol's artifact. A wrapper is built here rather than by the translator,
    /// so it carries only the sites this file emits into it — the id the stack guard takes, whose
    /// value is the entry's own function.
    fn capture_wrapper(
        &mut self,
        func: u32,
        role: JitSymbolRole,
        name: &str,
        unwind: cranelift_codegen::isa::unwind::UnwindInfo,
        lsda: Option<Vec<u8>>,
        cctx: &cranelift_codegen::Context,
    ) {
        if !self.reload && !self.jit_cache {
            return;
        }
        let symbol = cctx.compiled_code().and_then(|compiled| {
            artifact::capture(artifact::Captured {
                module: &self.module,
                func,
                role,
                name,
                ir_func: &cctx.func,
                compiled,
                sites: &self.sites,
                ordinals: self.ordinals.as_ref()?,
                unwind: Some(unwind),
                lsda,
            })
        });
        match symbol {
            Some(symbol) => self.artifacts.push(symbol),
            // A kind this engine cannot replay: this compiler keeps publishing the module's code, and a
            // partial entry is not storable either, because the loader links an entry whole.
            None => {
                self.reload = false;
                self.jit_cache = false;
                self.artifacts.clear();
            }
        }
    }

    pub(super) fn fast_sig(&mut self, abi: CalleeAbi) -> Signature {
        let mut sig = self.module.make_signature();
        for _ in 0..abi.nparams {
            sig.params.push(AbiParam::new(types::I64));
        }
        for _ in 0..abi.nrets {
            sig.returns.push(AbiParam::new(types::I64));
        }
        sig
    }

    /// Compile one function that crossed the threshold. A rejected or failed request
    /// stays interpreted, silently.
    pub(super) fn compile(&mut self, func: u32) {
        let started = helpers::ledger_on().then(std::time::Instant::now);
        let jit = &self.shared.jit;
        if self.domain == CodeDomain::Trace && jit.trace_enter.load(Ordering::Acquire) == 0 {
            // Without the boundary pin a trace body has no recorder to read, so
            // it must not be built at all; interpretation keeps recording through
            // TLS and stays correct.
            return;
        }
        if self.published.contains(&func) {
            return; // already compiled by this compiler, at this tier
        }
        let Some(body) = self.shared.module.funcs.get(func as usize) else {
            return;
        };
        if !admit(self.shared, body) {
            // Staying interpreted is the intended outcome for a non-admitted function,
            // not a failure; strict mode only records the set. Gated by MIRVM_JIT_DEBUG.
            if jit.sync && crate::options::jit_debug() {
                crate::diag_debug!(
                    Jit,
                    "f{func} not admitted ({})",
                    self.shared
                        .module
                        .funcs
                        .header(func as usize)
                        .unwrap()
                        .name()
                );
            }
            return;
        }
        // A stored entry is the whole product of a compile: reuse it before spending one. The callees
        // are pre-warmed either way, because a stored body calls through the same PLT slots.
        if self.jit_cache && self.load_cached(func, body) {
            return;
        }
        let abi = callee_abi(ir::FuncHeader::Owned(body)).expect("admit already checked the shape");
        self.artifacts.clear();

        // Silent-failure discipline: a compile failure keeps the function interpreted
        // and never writes a panic to stderr, because the differential oracle compares
        // stderr byte-for-byte and thread ids would pollute it. MIRVM_JIT_SYNC is the
        // exception: an admitted function that fails records the FAIL sentinel loudly.
        // TODO: report these errors through the log system once one exists.
        let Some((fast_id, fast_symbol)) = self.define_fast(func, body, abi) else {
            self.strict_fail(func);
            return;
        };
        let mut symbols = vec![fast_symbol];
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::FastBody) {
            return;
        }
        let Some((packed_id, packed_symbol)) = self.define_packed(func, body, abi, fast_id) else {
            self.strict_fail(func);
            return;
        };
        symbols.push(packed_symbol);
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::Packed) {
            return;
        }
        let Some(callees) = self.prepare_callees(func, body) else {
            self.strict_fail(func);
            return;
        };
        #[cfg(test)]
        if self.fail_after_symbol == Some(JitSymbolRole::C2i) {
            return;
        }
        if self.module.finalize_definitions().is_err() {
            self.strict_fail(func);
            return;
        }
        // Finalize resolved every site's name, and the module caches what it resolved: the values no
        // longer have to live, and the table must not grow with the process's compile count.
        reloc::forget(&self.values, func, self.sites.len());
        self.sites.clear();
        self.register_pending_eh_frames();
        self.publish_callees(callees);
        let mut ranges = self.finalized_symbol_ranges(symbols);

        let mut fast = self.module.get_finalized_function(fast_id) as u64;
        let mut packed = self.module.get_finalized_function(packed_id) as u64;
        // One entry for this function, built here and used by everyone below: the store keeps it, and
        // a reload links it back. Building it before either means the two can never disagree.
        let entry = self.finish_entry();
        if self.jit_cache
            && let Some(entry) = &entry
        {
            self.stage_entry(entry);
        }
        // `MIRVM_JIT_RELOAD`: what gets published is what the artifact linked back. The module's own
        // code stays in its arena — cranelift-jit releases nothing per function — but nothing calls
        // it, so the run proves the stored form rather than the session that produced it. The linked
        // region is process-lifetime, like every published entry.
        if self.reload
            && let Some(entry) = &entry
            && let Some(linked) = self.relink(entry, func)
            && let Some(body_entry) = linked.entry(JitSymbolRole::FastBody)
            && let Some(reloaded) = linked.entry(JitSymbolRole::Packed)
            && let Some(frames) = Self::linked_frames(&linked, entry)
        {
            fast = body_entry;
            packed = reloaded;
            ranges = linked_ranges(self.shared, func, &linked);
            // The published code is the linked region, so its CFA programs are registered at the
            // addresses the link placed them at: the module's own FDEs describe code nothing calls,
            // and an unwind out of a frame without one cannot find its caller. A link this session
            // cannot describe keeps the module's code instead, which is registered already.
            self.register_eh_frames(frames);
            std::mem::forget(linked);
        }
        // A capture that stopped early leaves the symbols it had already taken: they describe a
        // function this session publishes from the module instead.
        self.artifacts.clear();
        // Publish order: fast first (self-recursion and other compiled callers reach
        // it), then packed (only the interpreter can enter compiled code through it).
        // Every memory range is complete before these two Release stores; the perf map
        // is written only by an explicit stop.
        jit.publish_compiled_entries_for(self.domain, func, fast, packed, ranges);
        self.published.insert(func);
        if let Some(started) = started {
            // The time is read before the sizes are gathered: the ledger must not price the work it
            // does to describe itself.
            let micros = started.elapsed().as_micros() as u64;
            helpers::ledger_row(
                helpers::Built::Compiled,
                u64::from(artifact::Ordinals::of(body).map_or(0, |ordinals| ordinals.size())),
                entry.as_ref().map_or(0, |entry| {
                    entry
                        .symbols
                        .iter()
                        .map(|symbol| symbol.code.len() as u64)
                        .sum()
                }),
                micros,
            );
        }
        match self.tier {
            Tier::Baseline => helpers::tier_baseline(),
            Tier::Optimized => helpers::tier_optimized(),
        }
    }

    /// Define missing PLT entries without making them callable. Keep unpublished definitions so
    /// another caller can reuse them if this request fails before publication.
    fn prepare_callees(&mut self, func: u32, body: &ir::FuncBody) -> Option<Vec<u32>> {
        let jit = &self.shared.jit;
        let mut callees: Vec<(u32, CalleeAbi)> = Vec::new();
        for blk in &body.blocks {
            if let Terminator::Call {
                callee, args, ret, ..
            } = &blk.term
                && *callee != func
                && !callees.iter().any(|(c, _)| c == callee)
                && let Some(cabi) =
                    callee_abi(self.shared.module.funcs.header(*callee as usize).unwrap())
                && cabi.nparams == args.len() + usize::from(matches!(ret, RetDest::Indirect(_)))
            {
                callees.push((*callee, cabi));
            }
        }
        let mut pending = Vec::new();
        for (c, cabi) in callees {
            if jit.slots_for(self.domain).slots_fast[c as usize].load(Ordering::Acquire) == 0 {
                if !self.pending_callees.contains_key(&c) {
                    let symbol = self.define_c2i_trampoline(c, cabi)?;
                    self.pending_callees.insert(c, symbol);
                }
                pending.push(c);
            }
        }
        Some(pending)
    }

    /// All definitions and their unwind records must be finalized before any slot is visible.
    fn publish_callees(&mut self, callees: Vec<u32>) {
        for callee in callees {
            let symbol = self.pending_callees.remove(&callee).unwrap();
            let entry = self.module.get_finalized_function(symbol.id) as u64;
            let ranges = self.finalized_symbol_ranges(vec![symbol]);
            self.shared
                .jit
                .publish_c2i_entry_for(self.domain, callee, entry, ranges);
        }
    }

    /// Publish this function's code from the store, if the store holds an entry for its fragment under
    /// this compiler's key.
    ///
    /// The entry is compared field by field before anything is linked: the digest is a key, not a proof.
    /// A hit that links registers the stored unwind material at the loaded addresses and publishes the
    /// same two entries a compile would. Everything a doubt can do is a miss, which spends a compile
    /// and nothing else.
    pub(super) fn load_cached(&mut self, func: u32, body: &ir::FuncBody) -> bool {
        let started = helpers::ledger_on().then(std::time::Instant::now);
        let Some(ordinals) = artifact::Ordinals::of(body) else {
            return false;
        };
        let key = crate::store::jit::key(&ordinals.fragment(), &self.jit_key.digest());
        let bytes = match self.jit_index().lookup(&key) {
            crate::store::jit::Lookup::Entry(bytes) => bytes,
            crate::store::jit::Lookup::Absent => {
                helpers::cache_miss();
                return false;
            }
            crate::store::jit::Lookup::Bad(reason) => {
                helpers::cache_refused();
                if crate::options::jit_debug() {
                    crate::diag_debug!(Jit, "f{func} stored entry refused: {reason}");
                }
                return false;
            }
        };
        let entry = artifact::Entry::decode(&bytes)
            .ok()
            .filter(|entry| entry.fragment == ordinals.fragment() && entry.jit == self.jit_key);
        let Some(entry) = entry else {
            helpers::cache_refused();
            if crate::options::jit_debug() {
                crate::diag_debug!(
                    Jit,
                    "f{func} stored entry refused: not this fragment and key"
                );
            }
            return false;
        };
        let shared = self.shared;
        let roles = [JitSymbolRole::FastBody, JitSymbolRole::Packed];
        let Ok(linked) = artifact::link(&entry, &roles, |target| match target {
            // One import table, so what a helper name means here is what it meant when the entry was
            // written.
            artifact::Target::Named(name) => imports::whitelist()
                .get(name.as_ref())
                .map(|addr| *addr as u64),
            target => artifact::target_value(shared, func, &ordinals, target),
        }) else {
            helpers::cache_refused();
            return false;
        };
        // The stored CFA programs become FDEs at the addresses the link placed the symbols at, exactly
        // as a fresh compile registers its own. An entry whose frames this engine cannot all express
        // is refused: a partially covered link is a wrong unwind.
        let Some(frames) = Self::linked_frames(&linked, &entry) else {
            helpers::cache_refused();
            if crate::options::jit_debug() {
                crate::diag_debug!(Jit, "f{func} stored entry has no registrable frames");
            }
            return false;
        };
        let Some(callees) = self.prepare_callees(func, body) else {
            return false;
        };
        if !callees.is_empty() {
            if self.module.finalize_definitions().is_err() {
                return false;
            }
            self.register_pending_eh_frames();
            self.publish_callees(callees);
        }
        self.register_eh_frames(frames);
        let ranges = linked_ranges(shared, func, &linked);
        let (Some(body_entry), Some(packed)) = (
            linked.entry(JitSymbolRole::FastBody),
            linked.entry(JitSymbolRole::Packed),
        ) else {
            helpers::cache_refused();
            return false;
        };
        // Published code lives to process end, so the mapping must not be unmapped with the handle.
        std::mem::forget(linked);
        helpers::cache_hit();
        if crate::options::jit_debug() {
            crate::diag_debug!(
                Jit,
                "f{func} ({}) published from a stored entry",
                self.shared
                    .module
                    .funcs
                    .header(func as usize)
                    .unwrap()
                    .name()
            );
        }
        self.shared
            .jit
            .publish_compiled_entries_for(self.domain, func, body_entry, packed, ranges);
        self.published.insert(func);
        if let Some(started) = started {
            let micros = started.elapsed().as_micros() as u64;
            helpers::ledger_row(
                helpers::Built::Linked,
                u64::from(ordinals.size()),
                entry
                    .symbols
                    .iter()
                    .map(|symbol| symbol.code.len() as u64)
                    .sum(),
                micros,
            );
        }
        match self.tier {
            Tier::Baseline => helpers::tier_baseline(),
            Tier::Optimized => helpers::tier_optimized(),
        }
        true
    }

    /// This definition's CFA program, or `None` when this engine cannot express it.
    ///
    /// A frame without an FDE is a frame an unwind passes over, so a definition that has none is not
    /// published at all: the callers treat it like a failed definition and the function stays
    /// interpreted, which is the semantic reference.
    pub(super) fn registrable_unwind(
        &self,
        cctx: &cranelift_codegen::Context,
    ) -> Option<cranelift_codegen::isa::unwind::UnwindInfo> {
        cctx.compiled_code()
            .and_then(|cc| cc.create_unwind_info(self.module.isa()).ok().flatten())
            .filter(crate::vm::jit::unwind::is_registrable)
    }

    /// The CFA programs of a linked entry, at the addresses the link placed its symbols at.
    ///
    /// What a session publishes for a linked entry is the linked region, so these are the frames the
    /// unwinder has to find; the module's own FDEs describe code nothing calls. `None` when any
    /// symbol's stored program is missing or of a kind this engine cannot express: a link that covers
    /// only some of its frames is refused whole, because the frames it does not cover are ones an
    /// unwind would pass over.
    pub(super) fn linked_frames(
        linked: &artifact::Linked,
        entry: &artifact::Entry,
    ) -> Option<Vec<crate::vm::jit::unwind::Frame>> {
        entry
            .symbols
            .iter()
            .map(|symbol| {
                let address = linked.entry(symbol.role)?;
                let unwind = symbol.unwind.clone()?;
                crate::vm::jit::unwind::is_registrable(&unwind)
                    .then(|| (address, unwind, symbol.lsda.clone()))
            })
            .collect()
    }

    /// The store index, loaded once per compiler: what this session can reuse is what was there when it
    /// started, which is exactly the entries a previous session published.
    fn jit_index(&mut self) -> &crate::store::jit::Index {
        self.jit_index
            .get_or_insert_with(crate::store::jit::Index::load)
    }

    /// This function's entry, or `None` when there is nothing to store or link: no artifact was
    /// captured, or the body's references could not be numbered.
    fn finish_entry(&mut self) -> Option<artifact::Entry> {
        let fragment = self.ordinals.as_ref()?.fragment();
        Some(artifact::Entry {
            fragment,
            jit: self.jit_key.clone(),
            symbols: std::mem::take(&mut self.artifacts),
        })
    }

    /// Stage one entry for the store, publishing the batch once it is worth a file. The bytes are what
    /// a reader decodes, so the round trip through the encoder happens before anything is written.
    fn stage_entry(&mut self, entry: &artifact::Entry) {
        let Ok(bytes) = entry.encode() else { return };
        self.jit_staging
            .add(entry.fragment, self.jit_key.digest(), bytes);
        if self.jit_staging.worth_publishing() {
            let session = std::mem::take(&mut self.jit_staging);
            if let Err(error) = session.publish()
                && crate::options::jit_debug()
            {
                crate::diag_debug!(Jit, "cannot publish JIT entries: {error}");
            }
        }
    }

    /// Link one entry back, through the encoded form.
    ///
    /// The bytes are what a store holds, so the reload decodes what it encoded: a field the two sides
    /// disagree on cannot hide behind a same-session shortcut. Every relocation resolves against live
    /// state — the same answers the translator baked — and a target that does not resolve makes the
    /// whole link a miss, leaving the module's own code published.
    fn relink(&mut self, entry: &artifact::Entry, func: u32) -> Option<artifact::Linked> {
        let entry = artifact::Entry::decode(&entry.encode().ok()?).ok()?;
        let shared = self.shared;
        let ordinals = self.ordinals.as_ref()?;
        let linked = artifact::link(
            &entry,
            &[JitSymbolRole::FastBody, JitSymbolRole::Packed],
            |target| match target {
                // Every helper generated code may call is in the one import table, so what a name
                // means here is what it meant at compile time.
                artifact::Target::Named(name) => imports::whitelist()
                    .get(name.as_ref())
                    .map(|addr| *addr as u64),
                target => artifact::target_value(shared, func, ordinals, target),
            },
        )
        .ok()?;
        if crate::options::jit_debug() {
            crate::diag_debug!(Jit, "f{func} linked back from its artifact");
        }
        Some(linked)
    }

    /// Strict verification mode (MIRVM_JIT_SYNC): an admitted function that fails to
    /// compile records the FAIL sentinel loudly, and the SYNC waiter aborts on it. The
    /// non-strict path never reaches here, so the silent-failure discipline holds.
    pub(super) fn strict_fail(&self, func: u32) {
        let jit = &self.shared.jit;
        if jit.sync {
            crate::diag_debug!(
                Jit,
                "f{func} ({}) meets compilation threshold but failed to be compiled",
                self.shared
                    .module
                    .funcs
                    .header(func as usize)
                    .unwrap()
                    .name()
            );
            jit.slots_for(self.domain).slots[func as usize].store(FAIL_SENTINEL, Ordering::Release);
        }
    }

    /// Fast body: bytecode blocks -> CLIF, slots -> SSA variables under the "I64
    /// zero-extended to width" invariant. Any compile failure returns `None` and keeps
    /// the function interpreted: a panic must never pollute the stderr differential.
    pub(super) fn define_fast(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let sig = self.fast_sig(abi);
        let id = self
            .module
            .declare_function(&format!("f{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.take_context();
        cctx.func.signature = sig;
        // The Translator sets `has_try_call` while building; it is read outside the
        // builder to decide whether an LSDA is needed.
        let has_try_call;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let frame_offs = analyze_frame(body);
            let frame_ss = if !frame_offs.needs_frame() {
                None
            } else {
                Some(b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    // A forced 0-byte frame materializes as 1 byte; the offset is still
                    // counted as 0, so nothing changes semantically.
                    // frame_align > 16: cranelift's x86_64 stack base only guarantees
                    // 16-byte alignment and there is no dynamic realignment, so the slot
                    // carries (align - 16) extra bytes and the translator aligns the
                    // entry in code as (addr + align - 1) & -align. A `__m256d` local
                    // reaching the 32-byte precondition of `mem::zeroed` is the case
                    // that motivates this.
                    if body.frame_align > 16 {
                        body.frame_size + (body.frame_align - 16)
                    } else {
                        body.frame_size.max(1)
                    },
                    body.frame_align.trailing_zeros() as u8,
                )))
            };
            let mut tr = Translator {
                shared: self.shared,
                func,
                values: &self.values,
                sites: reloc::Sites::default(),
                block: 0,
                item: 0,
                domain: self.domain,
                module: &mut self.module,
                b: &mut b,
                vars: std::collections::HashMap::new(),
                frame_offs,
                frame_ss,
                unreachable: self.unreachable,
                c2i: self.c2i,
                call_main_catch: self.call_main_catch,
                memmove: self.memmove,
                memset: self.memset,
                memcmp: self.memcmp,
                div_zero: self.div_zero,
                volatile_load: self.volatile_load,
                volatile_store: self.volatile_store,
                call_indirect: self.call_indirect,
                tls_ref: self.tls_ref,
                call_foreign: self.call_foreign,
                call_builtin: self.call_builtin,
                alloc: self.alloc,
                terminate_abort: self.terminate_abort,
                call_terminate: self.call_terminate,
                unwind_resume: self.unwind_resume,
                exception_is_engine_fault: self.exception_is_engine_fault,
                trap: self.trap,
                simd_stmt: self.simd_stmt,
                simd_rv: self.simd_rv,
                poll_signals: self.poll_signals,
                stack_guard: self.stack_guard,
                stack_overflow: self.stack_overflow,
                host_syscall_trace: self.host_syscall_trace,
                exception_var: None,
                has_try_call: false,
                frame_base_var: None,
            };
            tr.build(func, body);
            has_try_call = tr.has_try_call;
            self.sites = std::mem::take(&mut tr.sites);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if crate::options::jit_debug() {
                crate::diag_debug!(Jit, "define_function failed: {e:#?}");
            }
            if crate::options::jit_debug_dump() {
                // A dump payload, not a diagnostic: `MIRVM_JIT_DEBUG_DUMP` owns the exact shape and
                // the general verbosity threshold must not silently drop it.
                crate::diag::instrument(format_args!(
                    "mirvm-jit-debug: CLIF dump of failed function f{func}:\n{}",
                    cctx.func.display()
                ));
            }
            return None;
        }
        let unwind = self.registrable_unwind(&cctx)?;
        // A function with a try_call gets an LSDA, and it must list every call site, handler-less ones
        // included; build_lsda explains why. The artifact keeps a copy, so a later session reconstructs
        // the same FDE and landing pads at the address it links the entry to.
        let lsda = has_try_call.then(|| build_lsda(&collect_call_sites(&cctx)));
        self.pending_unwind.push((id, unwind.clone(), lsda.clone()));
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        // The site table, read from the backend's relocation list while the code is still in hand. A
        // recorded site the list does not mention is an absolute nothing could replay, so the two
        // counts must agree.
        let placed = {
            let compiled = cctx.compiled_code()?;
            reloc::placed(&cctx.func, compiled, &self.sites)
        };
        // A recorded site is not always placed: the optimizer removes a block or a fold, and the
        // instruction with it, so the two counts are reported rather than compared — what a stored
        // entry replays is the relocation list, and every relocation that names a site has one.
        if crate::options::jit_debug() {
            crate::diag_debug!(
                Jit,
                "f{func} {} of {} recorded sites placed",
                placed.len(),
                self.sites.len()
            );
        }
        if self.reload || self.jit_cache {
            let name = format!("f{func}");
            // The body's own reference numbering: what an entry records instead of this session's
            // addresses and ids, and what makes it a store candidate at all.
            self.ordinals = artifact::Ordinals::of(body);
            let symbol = artifact::capture(artifact::Captured {
                module: &self.module,
                func,
                role: JitSymbolRole::FastBody,
                name: &name,
                ir_func: &cctx.func,
                compiled: cctx.compiled_code()?,
                sites: &self.sites,
                ordinals: self.ordinals.as_ref().expect("just set"),
                unwind: Some(unwind),
                lsda,
            });
            match symbol {
                // The artifact is complete whether or not the wrapper symbols could be captured: a
                // body whose wrappers were dropped stores what it has, and a session that links it
                // registers the CFA programs it carries (see `linked_frames`).
                Some(symbol) => {
                    #[cfg(test)]
                    {
                        // The body's own entry, independent of whether the wrappers could be captured:
                        // the tests link one back and compare two sessions' bytes.
                        self.last_entry = Some(artifact::Entry {
                            fragment: self
                                .ordinals
                                .as_ref()
                                .map(artifact::Ordinals::fragment)
                                .unwrap_or_default(),
                            jit: self.jit_key.clone(),
                            symbols: vec![symbol.clone()],
                        });
                    }
                    self.artifacts.push(symbol);
                }
                // A kind this engine cannot replay: this compiler keeps publishing the module's code,
                // and a partial entry is not storable either.
                None => {
                    self.reload = false;
                    self.jit_cache = false;
                    self.artifacts.clear();
                }
            }
        }
        self.shared.jit.record_sites(self.domain, func, placed);
        self.recycle_context(cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::FastBody,
                size,
            },
        ))
    }

    /// Packed entry: one interp i2c hop, and the one entry through which Rust enters a domain's
    /// compiled code.
    ///
    /// The plain domain's signature is `(ctx: *mut Ctx, args: *const u64, ret: *mut u64)`: every Rust
    /// caller (`dispatch::call_guest`, the thunks, `run_export`) reaches plain compiled code here and
    /// nowhere else, so this is where the domain pins the current thread's `Ctx` in the register
    /// Cranelift reserved for it. Everything it calls reads that register and never sets it. The
    /// trace domain's signature stays `(args: *const u64, ret: *mut u64)` and it sets nothing: its own
    /// trampoline pins the recorder and restores it afterwards, and the two conventions must not be
    /// confused.
    pub(super) fn define_packed(
        &mut self,
        func: u32,
        body: &ir::FuncBody,
        abi: CalleeAbi,
        fast: ClifFuncId,
    ) -> Option<(ClifFuncId, PendingJitSymbol)> {
        let plain = self.plain_pins_ctx();
        let mut sig = self.module.make_signature();
        for _ in 0..if plain { 3 } else { 2 } {
            sig.params.push(AbiParam::new(types::I64));
        }
        let id = self
            .module
            .declare_function(&format!("p{func}"), Linkage::Local, &sig)
            .unwrap();
        let mut cctx = self.take_context();
        cctx.func.signature = sig;
        {
            let mut b = FunctionBuilder::new(&mut cctx.func, &mut self.fbc);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            let ps = b.block_params(entry).to_vec();
            // Cranelift does not preserve a pinned register across a call (it is this build's
            // convention, not a callee-saved register), so this boundary -- the only one Rust enters
            // plain compiled code through -- saves the value Rust had, pins the `Ctx`, and puts the
            // old value back on the way out. The trace domain's trampoline does the same for the
            // recorder it pins.
            let saved = if plain {
                let saved = b.ins().get_pinned_reg(types::I64);
                b.ins().set_pinned_reg(ps[0]);
                Some(saved)
            } else {
                None
            };
            // The guard's first argument is this function's id, and an id is a program's number: it
            // goes through the site vocabulary so a stored entry replays it as the entry's own id.
            let fv = reloc::emit_site(
                &mut self.module,
                &self.values,
                &mut b,
                func,
                Site::Func(func),
                u64::from(func),
                &mut self.sites,
            );
            let need = b.ins().iconst(
                types::I64,
                super::super::guard::required_headroom(ir::FuncHeader::Owned(body)) as i64,
            );
            super::super::guard::emit(
                &mut self.module,
                &mut b,
                self.domain,
                fv,
                need,
                self.stack_guard,
                self.stack_overflow,
            );
            let (argp, retp) = if plain {
                (ps[1], ps[2])
            } else {
                (ps[0], ps[1])
            };
            let mut args: Vec<Value> = Vec::with_capacity(abi.nparams);
            for i in 0..abi.nparams {
                args.push(
                    b.ins()
                        .load(types::I64, MemFlagsData::trusted(), argp, (i * 8) as i32),
                );
            }
            let fref = self.module.declare_func_in_func(fast, b.func);
            let call = b.ins().call(fref, &args);
            let results: Vec<Value> = b.inst_results(call).to_vec();
            // Both (lo, hi) slots are always written, so shapes with sret/nrets = 0
            // store zero.
            let r0 = if abi.nrets >= 1 {
                results[0]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            let r1 = if abi.nrets >= 2 {
                results[1]
            } else {
                b.ins().iconst(types::I64, 0)
            };
            b.ins().store(MemFlagsData::trusted(), r0, retp, 0);
            b.ins().store(MemFlagsData::trusted(), r1, retp, 8);
            // The pinned register is this build's convention, not a callee-saved one, so the value
            // Rust had goes back before this boundary returns. A frame that unwinds past here is
            // destroyed rather than returned into, and the frames above it restore their own
            // registers through the unwinder, so the normal path is the one that needs this.
            if let Some(saved) = saved {
                b.ins().set_pinned_reg(saved);
            }
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize();
        }
        if let Err(e) = self.module.define_function(id, &mut cctx) {
            if crate::options::jit_debug() {
                crate::diag_debug!(Jit, "define_function failed: {e:#?}");
            }
            return None;
        }
        let unwind = self.registrable_unwind(&cctx)?;
        self.pending_unwind.push((id, unwind.clone(), None));
        let size = cctx.compiled_code()?.code_buffer().len() as u64;
        self.capture_wrapper(
            func,
            JitSymbolRole::Packed,
            &format!("p{func}"),
            unwind,
            None,
            &cctx,
        );
        self.recycle_context(cctx);
        Some((
            id,
            PendingJitSymbol {
                id,
                func,
                role: JitSymbolRole::Packed,
                size,
            },
        ))
    }
}

/// The perf-map ranges of linked code: the addresses the link produced, not the module's, with the
/// same roles and sizes so a profiler reads the running code.
fn linked_ranges(shared: &Shared, func: u32, linked: &artifact::Linked) -> Vec<JitSymbolRange> {
    let name = shared.module.funcs.header(func as usize).unwrap().name();
    linked
        .symbols
        .iter()
        .map(|symbol| {
            JitSymbolRange::new(shared.id, func, symbol.role, symbol.addr, symbol.size, name)
        })
        .collect()
}
