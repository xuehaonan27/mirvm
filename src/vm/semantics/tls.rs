//! The guest TLS instance table: one real address per `TlsId` per thread, materialized on
//! first access.
//!
//! The instance is allocated from the managed heap and filled from the frozen template, so a
//! guest address of a thread-local is again a real address. `Ctx` already is per-thread state,
//! which is what makes one table per `Ctx` the whole of the per-thread story; the thread-exit
//! chain reclaims both the guest destructors and the instance memory.

use crate::vm::ctx::Ctx;
use crate::vm::ir::Module;

/// True address of the current thread's instance of `id`, materialized on first access.
pub(crate) fn tls_addr(ctx: *mut Ctx, id: u32) -> u64 {
    let tls: &Vec<u64> = unsafe { &(*ctx).tls };
    if let Some(&a) = tls.get(id as usize)
        && a != 0
    {
        return a;
    }
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let t = module.tls[id as usize];
    let addr = crate::vm::heap::alloc(t.size.max(1), t.align as u64);
    unsafe {
        let template = (*(*ctx).shared).instance.resolve_link_addr(t.template);
        std::ptr::copy_nonoverlapping(template as *const u8, addr as *mut u8, t.size as usize);
        let tls = &mut (*ctx).tls;
        if tls.len() <= id as usize {
            tls.resize(id as usize + 1, 0);
        }
        tls[id as usize] = addr;
    }
    addr
}
