//! The managed heap as the guest sees it: the `__rust_alloc` family.
//!
//! The engine hands out real mimalloc addresses, so nothing is marshalled here. A module that
//! registered a `#[global_allocator]` takes the family over through its shim instead -- every
//! image's builtin arm must then route through the guest allocator, because freeing a mimalloc
//! pointer on the guest heap (or the reverse) corrupts mimalloc metadata.

use crate::vm::ctx::Ctx;
use crate::vm::dispatch::call_guest;
use crate::vm::ir::{Builtin, Module, UnwindAction};
use crate::vm::unwind::guarding_terminate;

pub(super) fn exec(
    builtin: &Builtin,
    ctx: *mut Ctx,
    module: &Module,
    av: &[u64],
    unwind: &UnwindAction,
) -> u64 {
    let a = |i: usize| av[i];
    match builtin {
        // Allocation sentinel: no-op.
        Builtin::NoAllocShim => 0,
        // Managed Rust heap (mimalloc backend; real addresses go straight out). Once a custom
        // #[global_allocator] is registered in this module, allocation is a program-level
        // semantic: every image's builtin arm routes through the guest shim to the user's
        // allocator, because a cross-heap free otherwise corrupts mimalloc metadata
        // (SIGSEGV).
        Builtin::RustAlloc => match module.custom_alloc_shims {
            Some(s) => {
                let (lo, _) =
                    guarding_terminate(unwind, || call_guest(ctx, s.alloc, &[a(0), a(1)]));
                lo
            }
            None => crate::vm::heap::alloc(a(0), a(1)),
        },
        Builtin::RustAllocZeroed => match module.custom_alloc_shims {
            Some(s) => {
                let (lo, _) =
                    guarding_terminate(unwind, || call_guest(ctx, s.alloc_zeroed, &[a(0), a(1)]));
                lo
            }
            None => crate::vm::heap::alloc_zeroed(a(0), a(1)),
        },
        Builtin::RustRealloc => match module.custom_alloc_shims {
            Some(s) => {
                let (lo, _) = guarding_terminate(unwind, || {
                    call_guest(ctx, s.realloc, &[a(0), a(1), a(2), a(3)])
                });
                lo
            }
            None => crate::vm::heap::realloc(a(0), a(1), a(2), a(3)),
        },
        Builtin::RustDealloc => {
            match module.custom_alloc_shims {
                Some(s) => {
                    let _ = guarding_terminate(unwind, || {
                        call_guest(ctx, s.dealloc, &[a(0), a(1), a(2)])
                    });
                }
                None => crate::vm::heap::dealloc(a(0), a(1), a(2)),
            }
            0
        }
        _ => unreachable!("non-alloc builtin reached the allocation family"),
    }
}
