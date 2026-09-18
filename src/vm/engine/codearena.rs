//! Entry-stub code region (P1, decision-history §7.6): fn-ptr values made executable.
//!
//! A fixed-base mmap in the same family as the three frozen regions: stable stub
//! offsets mean bytecode/frozen bytes can bake stub addresses (value domain is stable
//! across processes). Stub contents (`movabs rax, <closure code address>; jmp rax`)
//! contain the per-process random closure address — **not in snapshots**: rebuilt at
//! startup from the module's sites recipe (same contract as asm_sites/GOT), then the
//! whole region is filled and mprotected RX (W^X).
//! If the region is occupied that is a loud failure (wrong base = cliff); the cold-path
//! dynamic fallback still works in this process but is not serializable (same rule as
//! FrozenArena).

/// Spacing between stubs (16 B: movabs rax 10 B + jmp rax 2 B = up to 12 B, aligned to 16).
pub const STUB_STRIDE: u64 = 16;

/// Code-region capacity (virtually reserved; one stub entry per instance, 64 MiB >> any real workload).
const CODE_CAP: usize = 64 << 20;

/// Code-region base values, spline parameters, and whitelist criteria are centralized in
/// `super::addrlayout` (the shared constant layer).
use super::addrlayout::{BASE_CODE_ADDR, DELTA_CODE_ADDR, image_code_addr, is_valid_code_home};

/// Per-address-region stub area (one per module region; image regions are attached on absorb).
pub struct StubArena {
    base: *mut u8,
    used: usize,
    at_fixed_base: bool,
    home: usize,
}

impl Default for StubArena {
    /// serde skip placeholder (not mapped; replaced when rebuilt from the sites recipe at startup).
    fn default() -> Self {
        StubArena {
            base: std::ptr::null_mut(),
            used: 0,
            at_fixed_base: false,
            home: 0,
        }
    }
}

impl StubArena {
    /// Materialize at a fixed base (shared site-selection rule for lower cold path and startup rebuild):
    /// first try this region's fixed base (cacheability prerequisite); if occupied fall back to a
    /// dynamic base — semantics unchanged, only this process's output is not serializable
    /// (same as FrozenArena).
    pub fn new_at(home: usize) -> Self {
        if let Some(p) =
            crate::os::mem::map_fixed_preferred(home, CODE_CAP, crate::os::mem::Prot::RW)
        {
            return StubArena {
                base: p,
                used: 0,
                at_fixed_base: true,
                home,
            };
        }
        let base = crate::os::mem::map_anon(CODE_CAP, crate::os::mem::Prot::RW, false);
        assert!(!base.is_null(), "StubArena: mmap failed");
        StubArena {
            base,
            used: 0,
            at_fixed_base: false,
            home,
        }
    }

    /// Strict load (warm/image replay): Err if the fixed base is occupied — the caller
    /// treats this as a cache miss (bytecode baked this region's stub addresses, wrong
    /// base replay = cliff).
    pub fn map_fixed(home: usize) -> Result<Self, String> {
        assert!(is_valid_code_home(home), "StubArena restore region invalid: {home:#x}");
        let Some(p) = crate::os::mem::map_fixed_preferred(home, CODE_CAP, crate::os::mem::Prot::RW)
        else {
            return Err(format!("stub code region fixed base {home:#x} occupied"));
        };
        Ok(StubArena {
            base: p,
            used: 0,
            at_fixed_base: true,
            home,
        })
    }

    pub fn new() -> Self {
        Self::new_at(DELTA_CODE_ADDR)
    }

    pub fn new_base_image() -> Self {
        Self::new_at(BASE_CODE_ADDR)
    }

    pub fn new_image(k: usize) -> Self {
        Self::new_at(image_code_addr(k))
    }

    /// Address of the idx-th stub slot (does not bump — lower first computes addresses in
    /// allocation order and bakes values; the startup materializer reproduces the same
    /// addresses by calling alloc_stub in the same order).
    pub fn addr_of(&self, idx: u64) -> u64 {
        self.base as usize as u64 + idx * STUB_STRIDE
    }

    /// Allocate one stub slot (bump, STUB_STRIDE-aligned) and return its real address;
    /// bytes are filled at startup.
    pub fn alloc_stub(&mut self) -> u64 {
        let addr = self.base as usize + self.used;
        self.used += STUB_STRIDE as usize;
        assert!(
            self.used <= CODE_CAP,
            "StubArena: code region exhausted ({} MiB)",
            CODE_CAP >> 20
        );
        addr as u64
    }

    /// Startup fill bytes: `movabs rax, target; jmp rax`. addr must come from alloc_stub in this region.
    pub fn write_stub(&self, addr: u64, target: u64) {
        let bytes = crate::arch::x86_64::asmstub::emit_stub_bytes(target);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
    }

    /// Seal after fill: whole region RX (W^X). Write permission used during mapping ends here.
    pub fn seal(&self) {
        if self.base.is_null() || self.used == 0 {
            return;
        }
        crate::os::mem::protect(self.base, CODE_CAP, crate::os::mem::Prot::RX)
            .expect("StubArena: mprotect RX failed");
    }

    pub fn at_fixed_base(&self) -> bool {
        self.at_fixed_base
    }

    pub fn home(&self) -> usize {
        self.home
    }

    /// Serializable precondition for snapshot/skip decisions: if a module with sites has
    /// its code region off the fixed base, stub addresses are unstable across processes —
    /// cache is refused by the same rule as the frozen regions.
    pub fn is_mapped(&self) -> bool {
        !self.base.is_null()
    }
}

impl Drop for StubArena {
    fn drop(&mut self) {
        if !self.base.is_null() {
            unsafe { crate::os::mem::unmap(self.base, CODE_CAP) };
        }
    }
}

impl std::fmt::Debug for StubArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "StubArena {{ base: {:p}, used: {}, fixed: {} }}",
            self.base, self.used, self.at_fixed_base
        )
    }
}

// SAFETY: filled at startup and then sealed read-only/executable; shared with execution threads
// for the lifetime of the Module (same post-seal read-only rule as FrozenArena).
unsafe impl Send for StubArena {}
unsafe impl Sync for StubArena {}
