//! Frozen arena: true-address storage for statics, const pools and fn-ptr entries.
//!
//! Materialized during loading in two passes (allocate first, then fill, which breaks pointer cycles; see
//! lower), then shared read-only with the Module after publish. The exception is `static mut` and interior
//! mutability, which stay guest-writable through raw writes to real addresses that the engine does not
//! mediate. The arena is an RW mmap with fixed capacity (like GuestMemory/ByteRegion), so addresses are
//! stable for its whole life and relocation happens for real exactly once.
//!
//! **Fixed base.** Absolute addresses inside the arena (fn entries, statics pointing at each other,
//! bytecode-inlined consts, `fn_addrs` keys) are stable across processes only if the arena base is stable.
//! The arena is therefore mapped at a preferred address, the same idea as JVM CDS. If that address is
//! occupied, it falls back loudly to a dynamic base: the process still runs, it is just not cacheable.
//! A snapshot is the used-prefix bytes; restoring remaps at the fixed base and memcpys. Restore must happen
//! before the guest runs, and the snapshot is the clean state right after lowering -- runtime inputs such
//! as argv are not in it (see `Instance::finalize_entry_argv`).

/// Frozen arena capacity (virtually reserved; physical pages are allocated on touch).
const FROZEN_CAP: usize = 256 << 20;

/// Fixed-base numeric values and whitelist criteria live in `crate::os_arch::addrspace` (this pair's fixed-address layout); see that module header for the address-selection rationale and the domain model.
use crate::os_arch::addrspace::{
    BASE_IMAGE_FIXED_ADDR, DELTA_FIXED_ADDR, image_addr, is_valid_home,
};

pub struct FrozenArena {
    base: *mut u8,
    used: usize,
    at_fixed_base: bool,
    /// The fixed base of this arena's home domain (used for serde self-description; the intended domain is still recorded when dynamically falling back).
    home: usize,
    /// The address base used when this arena's contents were lowered. The runtime `base` of a dynamic instance may change, but the link base does not.
    link_base: usize,
}

/// A reusable frozen-arena image that can be instantiated repeatedly. Contains only clean bytes and the link-time base those bytes use; it does not occupy guest address space itself.
#[derive(Clone, Debug)]
pub struct FrozenSnapshot {
    home: usize,
    link_base: usize,
    bytes: Vec<u8>,
}

impl FrozenSnapshot {
    pub fn home(&self) -> usize {
        self.home
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Build a snapshot from the two fields its artifact wire form carries. `link_base` is left equal
    /// to `home`: only an arena at its fixed base can be snapshotted, so the link base equals the home
    /// domain by construction, and a snapshot that claimed otherwise would embed addresses that are
    /// wrong after re-mapping.
    fn from_home_and_bytes(home: usize, bytes: Vec<u8>) -> Result<Self, String> {
        if !is_valid_home(home) {
            return Err(format!("invalid frozen snapshot domain: {home:#x}"));
        }
        if bytes.len() > FROZEN_CAP {
            return Err("frozen snapshot exceeds arena capacity".into());
        }
        Ok(Self {
            home,
            link_base: home,
            bytes,
        })
    }
}

/// `Module.frozen` wire form: the `(home, bytes)` 2-tuple a mapped `FrozenArena` serialized as, so a
/// cached artifact keeps its layout. `FrozenSnapshot`'s own serde carries `link_base` for the package
/// document; here it is always `home`, and writing a field that cannot vary would only invalidate
/// existing entries.
pub(crate) fn serialize_module_frozen<S: serde::Serializer>(
    frozen: &Option<FrozenSnapshot>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match frozen {
        None => serializer.serialize_none(),
        Some(snapshot) => serializer.serialize_some(&(
            snapshot.home as u64,
            serde_bytes_shim::Bytes(&snapshot.bytes),
        )),
    }
}

pub(crate) fn deserialize_module_frozen<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<FrozenSnapshot>, D::Error> {
    let wire: Option<(u64, Vec<u8>)> = serde::Deserialize::deserialize(deserializer)?;
    wire.map(|(home, bytes)| {
        let home = usize::try_from(home)
            .map_err(|_| format!("frozen snapshot domain {home:#x} does not fit this host"))?;
        FrozenSnapshot::from_home_and_bytes(home, bytes)
    })
    .transpose()
    .map_err(serde::de::Error::custom)
}

impl Default for FrozenArena {
    fn default() -> Self {
        Self::new()
    }
}

impl FrozenArena {
    fn new_at(home: usize) -> Self {
        // Try this domain's fixed base first (prerequisite for cacheability); if occupied (concurrent tests / rare ASLR collision) fall back to a dynamic base—semantics unchanged, only this process's output is not serializable.
        if let Some(p) =
            crate::os::mem::map_fixed_preferred(home, FROZEN_CAP, crate::os::mem::Prot::RW)
        {
            return FrozenArena {
                base: p,
                used: 0,
                at_fixed_base: true,
                home,
                link_base: p as usize,
            };
        }
        let base = crate::os::mem::map_anon(FROZEN_CAP, crate::os::mem::Prot::RW, false);
        assert!(!base.is_null(), "FrozenArena: mmap failed");
        FrozenArena {
            base,
            used: 0,
            at_fixed_base: false,
            home,
            link_base: base as usize,
        }
    }

    /// Frozen arena for the program module (delta; equals the full module when there is no base image).
    pub fn new() -> Self {
        Self::new_at(DELTA_FIXED_ADDR)
    }

    /// For base-image build sessions only.
    pub fn new_base_image() -> Self {
        Self::new_at(BASE_IMAGE_FIXED_ADDR)
    }

    /// For dependency-image build sessions only; placed in the k-th spline domain.
    pub fn new_image(k: usize) -> Self {
        Self::new_at(image_addr(k))
    }

    /// Restore from a snapshot into the given domain (L2 warm / base image / dependency image load). Err if the fixed base is occupied—the caller treats this as a cache miss and never replays the snapshot at another base (snapshots embed absolute addresses; wrong base = silently wrong values).
    pub fn restore(snapshot: &[u8], home: usize) -> Result<Self, String> {
        assert!(
            snapshot.len() <= FROZEN_CAP,
            "frozen snapshot exceeds arena capacity"
        );
        assert!(
            is_valid_home(home),
            "invalid frozen restore domain: {home:#x}"
        );
        let Some(p) =
            crate::os::mem::map_fixed_preferred(home, FROZEN_CAP, crate::os::mem::Prot::RW)
        else {
            return Err(format!(
                "frozen fixed base {home:#x} is occupied; cannot restore snapshot"
            ));
        };
        unsafe {
            std::ptr::copy_nonoverlapping(snapshot.as_ptr(), p, snapshot.len());
        }
        Ok(FrozenArena {
            base: p,
            used: snapshot.len(),
            at_fixed_base: true,
            home,
            link_base: home,
        })
    }

    /// Create a separate runnable instance from a reusable image. Uses an anonymous address each time so that instances of the same artifact do not contend for the fixed mapping; absolute pointers in the image are later patched by the Module's FrozenReloc.
    pub fn restore_dynamic(snapshot: &FrozenSnapshot) -> Result<Self, String> {
        if snapshot.bytes.len() > FROZEN_CAP {
            return Err("frozen snapshot exceeds arena capacity".into());
        }
        if !is_valid_home(snapshot.home) {
            return Err(format!(
                "invalid frozen snapshot home: {:#x}",
                snapshot.home
            ));
        }
        let base = crate::os::mem::map_anon(FROZEN_CAP, crate::os::mem::Prot::RW, false);
        if base.is_null() {
            return Err("fail to map frozen instance".into());
        }
        unsafe {
            std::ptr::copy_nonoverlapping(snapshot.bytes.as_ptr(), base, snapshot.bytes.len());
        }
        Ok(Self {
            base,
            used: snapshot.bytes.len(),
            at_fixed_base: false,
            home: snapshot.home,
            link_base: snapshot.link_base,
        })
    }

    /// Whether this arena is at its fixed base (false ⇒ addresses are not stable across processes; serialization is forbidden).
    pub fn at_fixed_base(&self) -> bool {
        self.at_fixed_base
    }

    /// The fixed base of this arena's home domain (the loader checks that the base image really is in the
    /// base-image domain).
    pub fn home(&self) -> usize {
        self.home
    }

    pub fn link_base(&self) -> u64 {
        self.link_base as u64
    }

    pub fn runtime_base(&self) -> u64 {
        self.base as u64
    }

    pub fn used(&self) -> u64 {
        self.used as u64
    }

    /// Turn a fixed-address lower product into a package image that does not occupy a mapping.
    pub fn to_snapshot(&self) -> Result<FrozenSnapshot, String> {
        if !self.at_fixed_base {
            return Err("frozen arena is not at its link-time base".into());
        }
        Ok(FrozenSnapshot {
            home: self.home,
            link_base: self.link_base,
            bytes: self.snapshot().to_vec(),
        })
    }

    /// Snapshot = the used prefix (clean-state responsibility lies with the caller: snapshot before the guest runs).
    pub fn snapshot(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base, self.used) }
    }

    /// Bump allocation (aligned to `align`, zeroed), returning the real address.
    pub fn alloc(&mut self, size: u64, align: u64) -> u64 {
        let align = align.max(1) as usize;
        let aligned = (self.base as usize + self.used + align - 1) & !(align - 1);
        let start = aligned - self.base as usize;
        let end = start + size as usize;
        assert!(
            end <= FROZEN_CAP,
            "FrozenArena: frozen arena exhausted ({} MiB)",
            FROZEN_CAP >> 20
        );
        self.used = end;
        aligned as u64
    }
}

impl Drop for FrozenArena {
    fn drop(&mut self) {
        unsafe { crate::os::mem::unmap(self.base, FROZEN_CAP) };
    }
}

impl std::fmt::Debug for FrozenArena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FrozenArena {{ base: {:p}, used: {}, fixed: {} }}",
            self.base, self.used, self.at_fixed_base
        )
    }
}

// Serialization: an arena not at its fixed base holds addresses that are unstable across processes, so
// serialization must fail -- the upper layer reads that as "not cacheable this time" -- rather than emit a
// snapshot with silently wrong values. The encoding is self-describing as a (home, bytes) tuple: deserialize
// restores into the domain carried in the snapshot, and `restore` asserts the domain whitelist, so a forged
// snapshot cannot place the arena at an arbitrary address.
impl serde::Serialize for FrozenArena {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !self.at_fixed_base {
            return Err(serde::ser::Error::custom(
                "frozen arena is not at fixed base, cannot serialize",
            ));
        }
        use serde::ser::SerializeTuple;
        let mut t = serializer.serialize_tuple(2)?;
        t.serialize_element(&(self.home as u64))?;
        t.serialize_element(&serde_bytes_shim::Bytes(self.snapshot()))?;
        t.end()
    }
}

impl<'de> serde::Deserialize<'de> for FrozenArena {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // postcard bytes = borrowable slice; use &[u8] to avoid an intermediate copy
        let (home, bytes): (u64, &[u8]) = serde::Deserialize::deserialize(deserializer)?;
        let home = usize::try_from(home).map_err(serde::de::Error::custom)?;
        if !is_valid_home(home) {
            return Err(serde::de::Error::custom(format!(
                "invalid frozen snapshot domain: {home:#x}"
            )));
        }
        FrozenArena::restore(bytes, home).map_err(serde::de::Error::custom)
    }
}

impl serde::Serialize for FrozenSnapshot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeTuple;
        let mut t = serializer.serialize_tuple(3)?;
        t.serialize_element(&(self.home as u64))?;
        t.serialize_element(&(self.link_base as u64))?;
        t.serialize_element(&serde_bytes_shim::Bytes(&self.bytes))?;
        t.end()
    }
}

impl<'de> serde::Deserialize<'de> for FrozenSnapshot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (home, link_base, bytes): (u64, u64, Vec<u8>) =
            serde::Deserialize::deserialize(deserializer)?;
        let home = usize::try_from(home).map_err(serde::de::Error::custom)?;
        let link_base = usize::try_from(link_base).map_err(serde::de::Error::custom)?;
        if !is_valid_home(home) || link_base != home {
            return Err(serde::de::Error::custom(format!(
                "invalid frozen snapshot domain: home={home:#x}, link={link_base:#x}"
            )));
        }
        if bytes.len() > FROZEN_CAP {
            return Err(serde::de::Error::custom(
                "frozen snapshot exceeds arena capacity",
            ));
        }
        Ok(Self {
            home,
            link_base,
            bytes,
        })
    }
}

/// Tuple-embedded form of serialize_bytes: Serialize for &[u8] would serialize each u8 (varint per byte under postcard, worse size/speed)—wrap it to force the bytes channel.
mod serde_bytes_shim {
    pub struct Bytes<'a>(pub &'a [u8]);
    impl serde::Serialize for Bytes<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(self.0)
        }
    }
}

// SAFETY: the arena is read-only after publish — `static mut` writes go through raw addresses, not `&self` —
// and it is shared across execution threads for the Module's lifetime.
unsafe impl Send for FrozenArena {}
unsafe impl Sync for FrozenArena {}

#[cfg(test)]
mod tests {
    use std::sync::{LazyLock, Mutex};

    use super::FrozenArena;
    use crate::os_arch::addrspace::{
        BASE_IMAGE_FIXED_ADDR, DELTA_FIXED_ADDR, IMAGE_SPLINE_BASE, IMAGE_SPLINE_COUNT,
        IMAGE_SPLINE_STEP, image_addr, is_valid_home,
    };

    static FIXED_ADDRESS_TEST: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Spline-domain whitelist: base image, delta and aligned splines are valid; unaligned, out-of-bounds and
    /// stray addresses are invalid.
    #[test]
    fn image_spline_home_validation() {
        assert!(is_valid_home(BASE_IMAGE_FIXED_ADDR));
        assert!(is_valid_home(DELTA_FIXED_ADDR));
        assert!(is_valid_home(image_addr(0)));
        assert!(is_valid_home(image_addr(1)));
        assert!(is_valid_home(image_addr(IMAGE_SPLINE_COUNT - 1)));
        // unaligned (mid-spline) is invalid
        assert!(!is_valid_home(IMAGE_SPLINE_BASE + IMAGE_SPLINE_STEP / 2));
        // out-of-bounds is invalid
        assert!(!is_valid_home(
            IMAGE_SPLINE_BASE + IMAGE_SPLINE_COUNT * IMAGE_SPLINE_STEP
        ));
        // stray address is invalid (forged-snapshot defense)
        assert!(!is_valid_home(0x1234_5678));
        assert!(!is_valid_home(0x7f00_0000_0000));
        // splines do not touch the mmap top-down region
        assert!(image_addr(IMAGE_SPLINE_COUNT - 1) < 0x7f00_0000_0000);
        // domains are pairwise non-overlapping (FROZEN_CAP is far smaller than the step)
        assert!(image_addr(0) > DELTA_FIXED_ADDR);
        assert!(image_addr(1) - image_addr(0) == IMAGE_SPLINE_STEP);
    }

    /// Fixed-base snapshot/restore roundtrip: address values are stable, contents are byte-for-byte faithful, and allocation can continue after restore.
    #[test]
    fn snapshot_restore_roundtrip_preserves_addresses_and_bytes() {
        let _fixed_address = FIXED_ADDRESS_TEST.lock().unwrap();
        let mut a = FrozenArena::new();
        if !a.at_fixed_base() {
            // Concurrent tests took the fixed base—this test needs exclusive access, so skip (remaining assertions would be meaningless)
            eprintln!("skip: fixed base occupied");
            return;
        }
        let p = a.alloc(16, 8);
        let q = a.alloc(9, 1);
        unsafe {
            (p as *mut u64).write(0xdead_beef_cafe_f00d);
            // Typical frozen-arena shape: an embedded absolute pointer into the arena
            ((p + 8) as *mut u64).write(q);
            std::ptr::copy_nonoverlapping(
                c"mirvm-l2".to_bytes_with_nul().as_ptr(),
                q as *mut u8,
                9,
            );
        }
        let snap = a.snapshot().to_vec();
        drop(a);

        let b = FrozenArena::restore(&snap, DELTA_FIXED_ADDR).expect("restore failed");
        assert!(b.at_fixed_base());
        unsafe {
            assert_eq!((p as *const u64).read(), 0xdead_beef_cafe_f00d);
            let q2 = ((p + 8) as *const u64).read();
            assert_eq!(q2, q, "embedded absolute pointer must be bit-stable");
            assert_eq!(
                std::slice::from_raw_parts(q2 as *const u8, 9),
                b"mirvm-l2\0"
            );
        }
        // Append allocation after restore (argv finalization uses this path)
        let mut b = b;
        let r = b.alloc(8, 8);
        assert!(
            r >= q + 9,
            "appended allocation must lie after the snapshot"
        );
    }

    /// Dual-domain: base-image and delta arenas coexist, and cross-domain absolute pointers (delta→base, the
    /// common shape after a base lookup hits) are bit-stable after restore.
    #[test]
    fn dual_domain_arenas_coexist_and_cross_references_survive_restore() {
        let _fixed_address = FIXED_ADDRESS_TEST.lock().unwrap();
        let mut base = FrozenArena::new_base_image();
        let mut delta = FrozenArena::new();
        if !base.at_fixed_base() || !delta.at_fixed_base() {
            eprintln!("skip: fixed base occupied");
            return;
        }
        assert_eq!(
            (base.alloc(8, 8) & !0xffff_ffff) as usize,
            BASE_IMAGE_FIXED_ADDR
        );
        let b_cell = base.alloc(8, 8);
        unsafe { (b_cell as *mut u64).write(0x42) };
        // delta embeds an absolute pointer into the base arena (shape of fn entries / static deduplication)
        let d_ptr = delta.alloc(8, 8);
        unsafe { (d_ptr as *mut u64).write(b_cell) };

        let base_snap = base.snapshot().to_vec();
        let delta_snap = delta.snapshot().to_vec();
        drop(delta);
        drop(base);

        let _base2 = FrozenArena::restore(&base_snap, BASE_IMAGE_FIXED_ADDR)
            .expect("base image restore failed");
        let _delta2 =
            FrozenArena::restore(&delta_snap, DELTA_FIXED_ADDR).expect("delta restore failed");
        unsafe {
            let cross = (d_ptr as *const u64).read();
            assert_eq!(cross, b_cell, "cross-domain pointer is bit-stable");
            assert_eq!(
                (cross as *const u64).read(),
                0x42,
                "base content is readable through the cross-domain pointer"
            );
        }
    }
}
