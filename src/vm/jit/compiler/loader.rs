//! Persistent code loading owns no compiler module. ABI adapters are saved code too.

use super::*;

struct Key {
    value: artifact::JitKey,
    digest: [u8; 32],
}

struct Candidate {
    func: u32,
    tier: Tier,
    entry: artifact::Entry,
    ordinals: artifact::Ordinals,
    callees: Vec<(u32, CalleeAbi)>,
}

pub(super) struct Loader<'a> {
    shared: &'a Shared,
    domain: CodeDomain,
    isa: cranelift_codegen::isa::OwnedTargetIsa,
    keys: [Key; 2],
    index: Option<crate::store::jit::Index>,
    published: std::collections::HashMap<u32, Tier>,
}

impl<'a> Loader<'a> {
    pub(super) fn new(shared: &'a Shared, domain: CodeDomain) -> Self {
        let isa = isa_for(domain, Tier::Optimized);
        let keys = [Tier::Baseline, Tier::Optimized].map(|tier| {
            let value = artifact::JitKey::of(isa_for(domain, tier).as_ref(), domain);
            let digest = value.digest();
            Key { value, digest }
        });
        Self {
            shared,
            domain,
            isa,
            keys,
            index: None,
            published: Default::default(),
        }
    }

    pub(super) fn has(&self, func: u32, tier: Tier) -> bool {
        self.published
            .get(&func)
            .is_some_and(|current| *current == Tier::Optimized || *current == tier)
    }

    pub(super) fn record(&mut self, func: u32, tier: Tier) {
        self.published.insert(func, tier);
    }

    fn candidate(&mut self, func: u32, tier: Tier) -> Option<Candidate> {
        let body = self.shared.module.funcs.get(func as usize)?;
        let ordinals = artifact::Ordinals::of(body)?;
        let key = &self.keys[usize::from(tier == Tier::Optimized)];
        let index = self
            .index
            .get_or_insert_with(crate::store::jit::Index::load);
        let bytes = match index.lookup(&crate::store::jit::key(&ordinals.fragment(), &key.digest)) {
            crate::store::jit::Lookup::Entry(bytes) => bytes,
            crate::store::jit::Lookup::Absent => {
                helpers::cache_miss();
                return None;
            }
            crate::store::jit::Lookup::Bad(reason) => {
                helpers::cache_refused();
                if crate::options::jit_debug() {
                    crate::diag_debug!(Jit, "f{func} stored entry refused: {reason}");
                }
                return None;
            }
        };
        let entry = artifact::Entry::decode(&bytes).ok().filter(|entry| {
            entry.fragment == ordinals.fragment()
                && entry.jit == key.value
                && entry.symbols.len() == 2
                && entry.symbol(JitSymbolRole::FastBody).is_some()
                && entry.symbol(JitSymbolRole::Packed).is_some()
                && entry
                    .symbols
                    .iter()
                    .chain(entry.adapters.iter().map(|adapter| &adapter.symbol))
                    .all(|symbol| {
                        symbol
                            .unwind
                            .as_ref()
                            .is_some_and(crate::vm::jit::unwind::is_registrable)
                    })
        });
        let Some(entry) = entry else {
            helpers::cache_refused();
            return None;
        };
        let callees = translate::callees(self.shared, func, body, false);
        if callees.iter().any(|(_, abi)| {
            !entry.adapters.iter().any(|adapter| {
                adapter.params == abi.nparams
                    && adapter.returns == abi.nrets
                    && adapter.symbol.role == JitSymbolRole::C2i
            })
        }) {
            helpers::cache_refused();
            return None;
        }
        Some(Candidate {
            func,
            tier,
            entry,
            ordinals,
            callees,
        })
    }

    /// A historical request prefers optimized code but can reuse a saved baseline entry.
    /// Live requests ask for their exact tier. The caller bounds the batch to keep demand responsive.
    pub(super) fn load(&mut self, requests: &[(u32, Tier)], fallback: bool) -> Vec<u32> {
        if self.domain != CodeDomain::Plain || crate::options::no_jit_cache() {
            return Vec::new();
        }
        let started = helpers::ledger_on().then(std::time::Instant::now);
        let mut candidates = Vec::new();
        for &(func, tier) in requests {
            if self.has(func, tier) || self.shared.jit.stopping.load(Ordering::Acquire) {
                continue;
            }
            if let Some(candidate) = self.candidate(func, tier).or_else(|| {
                (fallback && tier == Tier::Optimized)
                    .then(|| self.candidate(func, Tier::Baseline))
                    .flatten()
            }) {
                candidates.push(candidate);
            }
        }
        if candidates.is_empty() {
            return Vec::new();
        }
        let slots = self.shared.jit.slots_for(self.domain);
        let mut adapters = std::collections::BTreeMap::new();
        for candidate in &candidates {
            for &(func, abi) in &candidate.callees {
                if slots.slots_fast[func as usize].load(Ordering::Acquire) == 0 {
                    adapters.entry(func).or_insert_with(|| {
                        &candidate
                            .entry
                            .adapters
                            .iter()
                            .find(|adapter| {
                                adapter.params == abi.nparams && adapter.returns == abi.nrets
                            })
                            .unwrap()
                            .symbol
                    });
                }
            }
        }
        let mut entries: Vec<Vec<&artifact::Symbol>> = candidates
            .iter()
            .map(|candidate| candidate.entry.symbols.iter().collect())
            .collect();
        entries.extend(adapters.values().map(|symbol| vec![*symbol]));
        let adapter_ids: Vec<u32> = adapters.keys().copied().collect();
        let linked = artifact::link_batch(&entries, |index, target| match target {
            artifact::Target::Named(name) => imports::whitelist()
                .get(name.as_ref())
                .map(|addr| *addr as u64),
            target if index < candidates.len() => {
                let candidate = &candidates[index];
                artifact::target_value(self.shared, candidate.func, &candidate.ordinals, target)
            }
            artifact::Target::OwnId => Some(u64::from(adapter_ids[index - candidates.len()])),
            _ => None,
        });
        let Ok(mut linked) = linked else {
            helpers::cache_refused();
            return Vec::new();
        };
        let frames = entries
            .iter()
            .zip(&linked)
            .flat_map(|(symbols, linked)| {
                symbols.iter().map(|symbol| {
                    (
                        linked.entry(symbol.role).unwrap(),
                        symbol.unwind.clone().unwrap(),
                        symbol.lsda.clone(),
                    )
                })
            })
            .collect();
        crate::vm::jit::unwind::register_frames(self.isa.as_ref(), frames);
        for (func, linked) in adapter_ids
            .into_iter()
            .zip(linked.drain(candidates.len()..))
        {
            let address = linked.entry(JitSymbolRole::C2i).unwrap();
            let ranges = translate::linked_ranges(self.shared, func, &linked);
            std::mem::forget(linked);
            self.shared
                .jit
                .publish_c2i_entry_for(self.domain, func, address, ranges);
        }
        let mut loaded = Vec::new();
        for (candidate, linked) in candidates.iter().zip(linked) {
            let fast = linked.entry(JitSymbolRole::FastBody).unwrap();
            let packed = linked.entry(JitSymbolRole::Packed).unwrap();
            let ranges = translate::linked_ranges(self.shared, candidate.func, &linked);
            std::mem::forget(linked);
            self.shared.jit.publish_compiled_entries_for(
                self.domain,
                candidate.func,
                fast,
                packed,
                ranges,
            );
            self.record(candidate.func, candidate.tier);
            helpers::cache_hit();
            match candidate.tier {
                Tier::Baseline => helpers::tier_baseline(),
                Tier::Optimized => helpers::tier_optimized(),
            }
            if crate::options::jit_debug() {
                crate::diag_debug!(Jit, "f{} published from a stored entry", candidate.func);
            }
            loaded.push(candidate.func);
        }
        if let Some(started) = started {
            helpers::ledger_row(
                helpers::Built::Linked,
                candidates
                    .iter()
                    .map(|candidate| u64::from(candidate.ordinals.size()))
                    .sum(),
                entries
                    .iter()
                    .flatten()
                    .map(|symbol| symbol.code.len() as u64)
                    .sum(),
                started.elapsed().as_micros() as u64,
            );
        }
        loaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_caller_installs_callee_adapter_without_a_compiler() {
        let module = ir::Module {
            funcs: vec![
                super::super::tests::body(
                    "caller",
                    Terminator::Call {
                        callee: 1,
                        args: Vec::new(),
                        ret: RetDest::Ignore,
                        role: ir::CallRole::Normal,
                        target: 1,
                        unwind: ir::UnwindAction::Continue,
                    },
                ),
                super::super::tests::body("callee", Terminator::Return),
            ]
            .into(),
            ..ir::Module::default()
        };
        let shared = Shared::new(module);
        let entry = {
            let mut compiler = Compiler::with_tier(&shared, CodeDomain::Plain, Tier::Optimized);
            compiler.reload = true;
            compiler.jit_cache = false;
            let body = &shared.module.funcs[0];
            let abi = callee_abi(ir::FuncHeader::Owned(body)).unwrap();
            let (fast, _) = compiler.define_fast(0, body, abi).unwrap();
            compiler.define_packed(0, body, abi, fast).unwrap();
            compiler.prepare_callees(0, body).unwrap();
            compiler.finish_entry(0, body).unwrap()
        };
        assert_eq!(entry.adapters.len(), 1);
        let dir = std::env::temp_dir().join(format!("mirvm-code-loader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = crate::store::jit::Session::default();
        session.add(entry.fragment, entry.jit.digest(), entry.encode().unwrap());
        session.publish_in(&dir).unwrap();
        let mut loader = Loader::new(&shared, CodeDomain::Plain);
        loader.index = Some(crate::store::jit::Index::load_in(&dir));
        assert_eq!(loader.load(&[(0, Tier::Optimized)], false), vec![0]);
        assert!(loader.has(0, Tier::Baseline));
        let slots = shared.jit.slots_for(CodeDomain::Plain);
        assert_ne!(slots.slots[0].load(Ordering::Acquire), 0);
        assert_ne!(slots.slots_fast[1].load(Ordering::Acquire), 0);
        assert_eq!(slots.slots[1].load(Ordering::Acquire), 0);
        assert!(loader.load(&[(0, Tier::Baseline)], false).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
