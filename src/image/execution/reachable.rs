//! The conservative function closure of an ordinary program entry. Every address-taken function
//! is a root, including entries in unused frozen data, so indirect/native calls need no guesses.

use crate::vm::instance::Instance;
use crate::vm::ir::{FuncHeader, FuncId, Module};

pub(super) fn functions(module: &Module, instance: &Instance) -> Vec<bool> {
    let Some(entry) = module.entry else {
        return vec![true; module.funcs.len()];
    };
    let mut live = vec![false; module.funcs.len()];
    let mut work = vec![entry.lang_start];
    work.extend(module.fn_entry_links.iter().map(|(_, id)| *id));
    work.extend(instance.link_fn_addrs.values().copied());
    work.extend(instance.fn_addrs.values().copied());
    work.extend(module.entry_stub_sites.iter().map(|site| site.func));
    work.extend(
        instance
            .image_entry_stubs
            .iter()
            .flat_map(|(_, sites, _)| sites.iter().map(|site| site.func)),
    );
    if let Some(shims) = module.custom_alloc_shims {
        work.extend([
            shims.alloc,
            shims.dealloc,
            shims.realloc,
            shims.alloc_zeroed,
        ]);
    }
    if let Some(panic) = module.guest_panic_cleanup {
        work.extend([panic.cleanup, panic.drop_payload]);
    }
    if crate::options::timing() {
        let roots: std::collections::HashSet<_> = work.iter().copied().collect();
        crate::diag::instrument(format_args!(
            "mirvm-image-roots: functions={} roots={}",
            live.len(),
            roots.len()
        ));
    }
    while let Some(id) = work.pop() {
        // The publisher verifies the complete module before computing its closure.
        if std::mem::replace(&mut live[id as usize], true) {
            continue;
        }
        match module
            .funcs
            .header(id as usize)
            .expect("verified function id")
        {
            FuncHeader::Owned(body) => {
                work.extend(
                    body.blocks
                        .iter()
                        .filter_map(|block| owned::callee(&block.term)),
                );
                if let Some(shared) = &body.shared {
                    work.extend(shared.calls.iter().filter_map(owned::callee));
                }
            }
            FuncHeader::Archived(body) => {
                work.extend(
                    body.blocks
                        .iter()
                        .filter_map(|block| archived::callee(&block.term)),
                );
                if let Some(shared) = body.shared.as_ref() {
                    work.extend(shared.calls.iter().filter_map(archived::callee));
                }
            }
        }
    }
    live
}

// Compile the same exhaustive edge walk for owned and archived instructions. Template call
// ordinals are not FuncIds; their concrete adapters above supply the edges instead.
macro_rules! direct_call {
    () => {
        pub(super) fn callee(term: &Terminator) -> Option<FuncId> {
            match term {
                Terminator::Call { callee, .. } => Some(*callee),
                Terminator::Goto(_)
                | Terminator::SwitchInt { .. }
                | Terminator::CallBuiltin { .. }
                | Terminator::CallForeign { .. }
                | Terminator::CallIndirect { .. }
                | Terminator::InlineAsm { .. }
                | Terminator::Return
                | Terminator::Unreachable
                | Terminator::Resume
                | Terminator::TerminateAbort
                | Terminator::Trap(_) => None,
            }
        }
    };
}

mod owned {
    use super::FuncId;
    use crate::vm::ir::Terminator;
    direct_call!();
}

mod archived {
    use super::FuncId;
    use crate::vm::ir::ArchivedTerminator as Terminator;
    direct_call!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::archive::{bytes::Bytes, functions as archive};
    use crate::vm::ir::*;
    use std::sync::Arc;

    fn call(callee: FuncId) -> Terminator {
        Terminator::Call {
            callee,
            args: Vec::new(),
            ret: RetDest::Ignore,
            target: 0,
            unwind: UnwindAction::Continue,
            role: CallRole::Normal,
        }
    }

    #[test]
    fn entry_closure_keeps_callbacks_runtime_roots_and_shared_call_adapters() {
        let mut bodies: Vec<_> = (0..17)
            .map(|id| FuncBody {
                shared: None,
                frame_size: 0,
                frame_align: 1,
                ret: RetAbi::Zst,
                params: Vec::new(),
                caller_loc_off: None,
                name: format!("function{id}").into(),
                blocks: vec![Block {
                    stmts: Vec::new(),
                    term: Terminator::Return,
                }],
            })
            .collect();
        bodies[0].blocks[0].term = call(2);
        bodies[2].blocks[0].term = call(0);
        bodies[3].blocks[0].term = call(14);
        bodies[14] = test_generic_body(7);
        bodies[14].shared.as_mut().unwrap().calls.push(call(15));
        let site = |func| EntryStubSite {
            link_addr: LinkAddr(100 + u64::from(func)),
            func,
            sig: ForeignSig {
                args: Vec::new(),
                ret: FfiKind::Void,
                fixed: None,
                thunk_args: Vec::new(),
                unwind: false,
            },
        };
        let mut module = Module {
            funcs: bodies.into(),
            exports: [("unused_export".into(), 1)].into(),
            fn_entry_links: vec![(LinkAddr(103), 3)],
            entry_stub_sites: vec![site(6)],
            custom_alloc_shims: Some(AllocShims {
                alloc: 8,
                dealloc: 9,
                realloc: 10,
                alloc_zeroed: 11,
            }),
            guest_panic_cleanup: Some(GuestPanicCleanup {
                cleanup: 12,
                drop_payload: 13,
            }),
            entry: Some(EntryPlan {
                lang_start: 0,
                main_addr: LinkAddr(103),
                argc: 0,
                argv_ptr: 0,
                sigpipe: 0,
            }),
            ..Default::default()
        };
        let instance = Instance {
            link_fn_addrs: [(LinkAddr(104), 4)].into(),
            fn_addrs: [(105, 5)].into(),
            image_entry_stubs: vec![(0, vec![site(7)], Default::default())],
            ..Default::default()
        };
        let mut expected = vec![true; 17];
        expected[1] = false;
        expected[16] = false;
        assert_eq!(functions(&module, &instance), expected);
        let bytes = archive::encode(&module.funcs).unwrap();
        module.funcs = FuncTable::from_archive(Arc::new(
            archive::Functions::load(Bytes::new(bytes.to_vec()), 0..bytes.len()).unwrap(),
        ));
        assert_eq!(functions(&module, &instance), expected);
        assert_eq!(module.funcs.decoded_count(), 0);
        module.entry = None;
        assert!(functions(&module, &instance).into_iter().all(|keep| keep));
    }
}
