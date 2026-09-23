//! Self-mapping the image: one anonymous span sized to the `PT_LOAD` segments, those segments'
//! bytes copied in, and a single load bias that turns every ELF virtual address the other phases
//! name into a real one.
//!
//! The span stays writable through relocation; the final per-segment protections are applied only
//! after the last relocation has been written. A mapping that never reaches an `McImage` is
//! unmapped by its drop, so a failed phase leaves nothing behind.

use crate::native::elf;

use super::bad;
use super::parse::Image;

/// The granularity the segments are mapped at. `PT_LOAD` virtual addresses are page-aligned by
/// the linker, so the span and the protection ranges round outward to this.
const PAGE: usize = 4096;

/// One `PT_LOAD` segment: its file range, its virtual range and its access flags.
#[derive(Clone, Copy)]
pub(super) struct Load {
    pub offset: u64,
    pub vaddr: usize,
    pub filesz: usize,
    pub memsz: usize,
    pub flags: u32,
}

/// The image's loadable segments in memory, at an address the loader chose.
pub(super) struct Mapping {
    raw: *mut u8,
    size: usize,
    load_bias: usize,
    loads: Box<[Load]>,
    dynamic: Option<(u64, u64)>,
    armed: bool,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if self.armed {
            unsafe { crate::os::mem::unmap(self.raw, self.size) };
        }
    }
}

impl Mapping {
    /// Map a span covering every `PT_LOAD` segment and copy the segment bytes into it. Refuses a
    /// `PT_INTERP` (a self-produced shared object has no interpreter) and an image with no
    /// loadable segment.
    pub(super) fn map(bytes: &[u8], image: &Image<'_>) -> Result<Self, String> {
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        let mut loads = Vec::new();
        let mut dynamic = None;
        for header in image.program_headers.iter() {
            match header.ty {
                elf::PT_LOAD => {
                    let vaddr = header.vaddr as usize;
                    let filesz = header.filesz as usize;
                    let memsz = header.memsz as usize;
                    if memsz < filesz || header.offset as usize + filesz > bytes.len() {
                        return Err(bad());
                    }
                    loads.push(Load {
                        offset: header.offset,
                        vaddr,
                        filesz,
                        memsz,
                        flags: header.flags,
                    });
                    lo = lo.min(vaddr & !(PAGE - 1));
                    hi = hi.max((vaddr + memsz + PAGE - 1) & !(PAGE - 1));
                }
                elf::PT_DYNAMIC => dynamic = Some((header.vaddr, header.memsz)),
                elf::PT_INTERP => {
                    return Err("MC image has PT_INTERP (not a self-produced shared object)".into());
                }
                _ => {}
            }
        }
        if loads.is_empty() || lo >= hi {
            return Err("MC image has no PT_LOAD".into());
        }
        let size = hi - lo;
        let raw = crate::os::mem::map_anon(size, crate::os::mem::Prot::RW, false);
        if raw.is_null() {
            return Err(format!("MC image mapping failed ({size:#x} bytes)"));
        }
        let mut mapping = Self {
            raw,
            size,
            load_bias: 0,
            loads: loads.into_boxed_slice(),
            dynamic,
            armed: true,
        };
        mapping.load_bias = (raw as usize)
            .checked_sub(lo)
            .ok_or("MC image mapping lies below its first ELF virtual address")?;
        for load in &mapping.loads {
            let destination = mapping.address(load.vaddr as u64, "PT_LOAD")?;
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr().add(load.offset as usize),
                    destination as *mut u8,
                    load.filesz,
                );
                if load.memsz > load.filesz {
                    std::ptr::write_bytes(
                        (destination + load.filesz) as *mut u8,
                        0,
                        load.memsz - load.filesz,
                    );
                }
            }
        }
        Ok(mapping)
    }

    /// The `PT_DYNAMIC` program header's virtual address and byte size.
    pub(super) fn dynamic(&self) -> Option<(u64, u64)> {
        self.dynamic
    }

    /// The real address of ELF virtual address `vaddr`, with `what` naming the caller's structure
    /// in the error.
    pub(super) fn address(&self, vaddr: u64, what: &str) -> Result<usize, String> {
        self.load_bias
            .checked_add(
                usize::try_from(vaddr)
                    .map_err(|_| format!("MC {what} virtual address does not fit usize"))?,
            )
            .ok_or_else(|| format!("MC {what} virtual address overflow"))
    }

    /// The real address of a signed relocation value.
    pub(super) fn signed_address(&self, value: i64, what: &str) -> Result<u64, String> {
        let address = if value >= 0 {
            self.load_bias.checked_add(value as usize)
        } else {
            self.load_bias.checked_sub(value.unsigned_abs() as usize)
        }
        .ok_or_else(|| format!("MC {what} signed address overflow"))?;
        Ok(address as u64)
    }

    /// Whether `[address, address + size)` lies inside one loadable segment.
    pub(super) fn contains(&self, address: u64, size: u64) -> bool {
        let Some(end) = address.checked_add(size) else {
            return false;
        };
        self.loads.iter().any(|load| {
            let start = load.vaddr as u64;
            start
                .checked_add(load.memsz as u64)
                .is_some_and(|load_end| address >= start && end <= load_end)
        })
    }

    /// Apply the final segment protections and refuse a segment that is both writable and
    /// executable, a shape the self-produced family never has.
    pub(super) fn protect(&self) -> Result<(), String> {
        for load in &self.loads {
            if load.flags & (elf::PF_X | elf::PF_W) == (elf::PF_X | elf::PF_W) {
                return Err("MC image contains a writable executable PT_LOAD segment".into());
            }
            let start = self.address(load.vaddr as u64, "PT_LOAD protection")? & !(PAGE - 1);
            let segment_end = self
                .address(load.vaddr as u64, "PT_LOAD protection")?
                .checked_add(load.memsz)
                .ok_or("MC PT_LOAD protection range overflow")?;
            let end = segment_end
                .checked_add(PAGE - 1)
                .ok_or("MC PT_LOAD protection alignment overflow")?
                & !(PAGE - 1);
            crate::os::mem::protect(start as *mut u8, end - start, seg_prot(load.flags))
                .map_err(|e| format!("MC image segment protection failed: {e}"))?;
        }
        Ok(())
    }

    /// The real `[start, end)` of every executable segment, the ranges a guest code pointer must
    /// lie in.
    pub(super) fn executable_ranges(&self) -> Result<Box<[(usize, usize)]>, String> {
        self.loads
            .iter()
            .filter(|load| load.flags & elf::PF_X != 0)
            .map(|load| {
                let start = self.address(load.vaddr as u64, "executable PT_LOAD")?;
                let end = start
                    .checked_add(load.memsz)
                    .ok_or("MC executable PT_LOAD range overflow")?;
                Ok((start, end))
            })
            .collect::<Result<Vec<_>, String>>()
            .map(Vec::into_boxed_slice)
    }

    /// Hand the span to the `McImage` that owns it, so the guard no longer unmaps it.
    pub(super) fn into_parts(mut self) -> (usize, usize, usize) {
        self.armed = false;
        (self.raw as usize, self.load_bias, self.size)
    }
}

/// A segment's final protection. Every loadable segment here is either executable or data, so an
/// executable one becomes read-execute and any other read-write.
fn seg_prot(flags: u32) -> crate::os::mem::Prot {
    if flags & elf::PF_X != 0 {
        crate::os::mem::Prot::RX
    } else {
        crate::os::mem::Prot::RW
    }
}
