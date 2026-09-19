//! Spike 1b: guest memory is **real addresses + bare access** -- no AllocId, no checker
//! overlay.
//!
//! This is exactly the model tier-0's InterpCx checker rejects: reading a real-address
//! pointer without an AllocId is judged `DanglingIntPointer`. Once the overlay is dropped, a
//! guest pointer is a real host address read and written bare; this file is the minimal
//! skeleton of that.
//!
//! Bump allocation, no free (skeleton; the real implementation uses a TLAB plus mimalloc).

/// A fixed-size anonymous mapping; the "pointer" handed to the guest is the real host
/// address.
pub struct GuestMemory {
    base: *mut u8,
    size: usize,
    offset: usize,
}

impl GuestMemory {
    pub fn new(size: usize) -> Self {
        // Real-address memory: anonymous RW mmap. Once the pointer is handed to the guest it
        // is read and written bare at its real address -- mirvm registers no AllocId and does
        // no range checks (fast).
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert!(base != libc::MAP_FAILED, "GuestMemory: mmap failed");
        GuestMemory {
            base: base as *mut u8,
            size,
            offset: 0,
        }
    }

    /// Bump-allocate `size` bytes (8-aligned); returns the real address.
    pub fn alloc(&mut self, size: u64) -> u64 {
        let aligned = (self.offset + 7) & !7;
        let end = aligned + size as usize;
        assert!(
            end <= self.size,
            "GuestMemory: overflow (bump allocation, no free)"
        );
        self.offset = end;
        (self.base as u64) + aligned as u64
    }

    /// Bare-read a u64 (real address, no AllocId check).
    ///
    /// # Safety
    /// `addr` must be inside this region and initialized. The fast machine assumes a
    /// well-behaved guest; out-of-bounds is guest UB.
    pub unsafe fn load(&self, addr: u64) -> u64 {
        unsafe { (addr as *const u64).read_unaligned() }
    }

    /// Bare-write a u64 (real address, no AllocId check).
    ///
    /// # Safety
    /// Same as [`load`](Self::load).
    pub unsafe fn store(&self, addr: u64, val: u64) {
        unsafe { (addr as *mut u64).write_unaligned(val) }
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.size) };
    }
}
