//! Addressing and the memory model: the reserved operand region, the width-masked read and write
//! the interpreter uses for every guest access, and the place/operand evaluation that turns an
//! expression into an address.

use super::*;

#[inline]
pub(crate) fn region_reserve(ctx: *mut Ctx, size: u32, align: u32) -> usize {
    let r: &mut ByteRegion = unsafe { &mut (*ctx).region };
    r.reserve(size, align)
}
#[inline]
pub(crate) fn region_restore(ctx: *mut Ctx, base: usize) {
    let r: &mut ByteRegion = unsafe { &mut (*ctx).region };
    r.restore(base);
}
#[inline]
pub(crate) fn slot_read(ctx: *mut Ctx, base: usize, s: Slot) -> u64 {
    let r: &ByteRegion = unsafe { &(*ctx).region };
    r.read(base, s)
}
#[inline]
pub(crate) fn slot_write(ctx: *mut Ctx, base: usize, s: Slot, v: u64) {
    let r: &mut ByteRegion = unsafe { &mut (*ctx).region };
    r.write(base, s, v);
}

/// Raw read through a real address: the guest's validity assumption is taken as given, with
/// no range check (the real-address model).
#[inline]
pub(crate) fn mem_read(addr: u64, w: Width) -> u64 {
    let p = addr as *const u8;
    unsafe {
        match w {
            Width::W8 => p.read_unaligned() as u64,
            Width::W16 => (p as *const u16).read_unaligned() as u64,
            Width::W32 => (p as *const u32).read_unaligned() as u64,
            Width::W64 => (p as *const u64).read_unaligned(),
        }
    }
}

/// Raw write through a real address, with the same no-range-check fast model as `mem_read`.
#[inline]
pub(crate) fn mem_write(addr: u64, w: Width, v: u64) {
    let p = addr as *mut u8;
    unsafe {
        match w {
            Width::W8 => p.write_unaligned(v as u8),
            Width::W16 => (p as *mut u16).write_unaligned(v as u16),
            Width::W32 => (p as *mut u32).write_unaligned(v as u32),
            Width::W64 => (p as *mut u64).write_unaligned(v),
        }
    }
}

/// True address of a guest TLS instance, materialized lazily on first access (heap
/// allocation plus a copy of the frozen template). One instance per thread because the Ctx
/// is thread-local; the thread-exit chain reclaims both the guest destructors and the
/// instance memory.
pub(crate) fn tls_addr(ctx: *mut Ctx, id: u32) -> u64 {
    let tls: &Vec<u64> = unsafe { &(*ctx).tls };
    if let Some(&a) = tls.get(id as usize)
        && a != 0
    {
        return a;
    }
    let module: &Module = unsafe { &(*(*ctx).shared).module };
    let t = module.tls[id as usize];
    let addr = super::super::heap::alloc(t.size.max(1), t.align as u64);
    unsafe {
        let template = module.resolve_link_addr(t.template);
        std::ptr::copy_nonoverlapping(template as *const u8, addr as *mut u8, t.size as usize);
        let tls = &mut (*ctx).tls;
        if tls.len() <= id as usize {
            tls.resize(id as usize + 1, 0);
        }
        tls[id as usize] = addr;
    }
    addr
}

/// Evaluates a place expression to a true address (the core of place evaluation; the frame
/// base is already a true address, so all arithmetic is on raw addresses).
pub(crate) fn eval_place_addr(ctx: *mut Ctx, base: usize, expr: &PlaceExpr) -> u64 {
    let mut addr = match expr.base {
        PlaceBase::Local(off) => base as u64 + off as u64,
        PlaceBase::Static(a) => unsafe { &(*(*ctx).shared).module }.resolve_link_addr(a),
    };
    for step in &expr.steps {
        match step {
            PlaceStep::Deref => addr = mem_read(addr, Width::W64),
            PlaceStep::Offset(o) => addr = addr.wrapping_add(*o as i64 as u64),
            PlaceStep::VTableAlignOffset {
                meta,
                unaligned,
                packed,
            } => {
                let (vtable, _) = eval_operand(ctx, base, meta);
                let mut align = mem_read(vtable.wrapping_add(2 * 8), Width::W64);
                if let Some(packed) = packed {
                    align = align.min(*packed);
                }
                if align == 0 || !align.is_power_of_two() {
                    engine_abort(&format!(
                        "dyn vtable alignment is not a power of two: {align} (vtable={vtable:#x})"
                    ));
                }
                let offset = unaligned.checked_add(align - 1).unwrap_or_else(|| {
                    engine_abort(&format!(
                        "dyn trailing-field offset overflow: unaligned={unaligned} align={align}"
                    ))
                }) & !(align - 1);
                addr = addr.wrapping_add(offset);
            }
            PlaceStep::IndexScaled { idx, stride } => {
                let i = slot_read(ctx, base, *idx);
                addr = addr.wrapping_add(i.wrapping_mul(*stride));
            }
        }
    }
    addr
}

pub(crate) fn eval_operand(ctx: *mut Ctx, base: usize, op: &Operand) -> (u64, Width) {
    match op {
        Operand::Slot(s) => (slot_read(ctx, base, *s), s.width),
        Operand::Mem { expr, width } => {
            let addr = eval_place_addr(ctx, base, expr);
            (mem_read(addr, *width), *width)
        }
        Operand::Imm { bits, width } => (*bits, *width),
        Operand::AddrImm(addr) => (
            unsafe { &(*(*ctx).shared).module }.resolve_link_addr(*addr),
            Width::W64,
        ),
        Operand::AddrOf(expr) => (eval_place_addr(ctx, base, expr), Width::W64),
        Operand::SubImm { base: b, sub } => {
            let (v, w) = eval_operand(ctx, base, b);
            (v.wrapping_sub(*sub) & w.mask(), w)
        }
    }
}

pub(crate) fn place_write(ctx: *mut Ctx, base: usize, p: &ScalarPlace, v: u64) {
    match p {
        ScalarPlace::Slot(s) => slot_write(ctx, base, *s, v),
        ScalarPlace::Mem { expr, width } => {
            let addr = eval_place_addr(ctx, base, expr);
            mem_write(addr, *width, v);
        }
    }
}
