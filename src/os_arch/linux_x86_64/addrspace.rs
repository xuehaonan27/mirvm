//! The numbers of this pair's fixed-address layout.
//!
//! The structure and every predicate over it are [`crate::os_arch::addrspace`]'s, because they hold
//! for every pair. What is here is the one thing that does not: where Linux on x86_64 leaves a hole
//! wide enough for the three frozen data regions and the three code-region families.
//!
//! Site-selection rationale (Linux x86_64 virtual address space knowledge, same premise as the
//! global_asm/asm-stub x86_64 hard floor): PIE image/brk randomization upper bound
//! ~0x66xx_xxxx_xxxx (mmap_rnd_bits=28), the top-down mmap region is near 0x7fxx_xxxx_xxxx — the
//! 0x68–0x6E band falls in the hole between the two and does not intersect the shadow IP region
//! (FUNC_IP_BASE, non-canonical high bits, never mapped). The spline step of 16 GiB is far larger
//! than each region's capacity (the hole allows future expansion), and the upper bound of 1300
//! stays below 0x7f.

// ---- Three frozen data regions (data: statics/constant pool/fn-ptr entries; S4 dual regions + S3' dependency splines) ----

/// Fixed base of the base image region (std pre-lowered modules shared across programs).
pub const BASE_IMAGE_FIXED_ADDR: usize = 0x6800_0000_0000;
/// Fixed base of the delta region (this program's modules; equals the full set when no base image exists).
pub const DELTA_FIXED_ADDR: usize = 0x6900_0000_0000;

/// Dependency image region spline: each registry dependency image occupies one fixed
/// region, starting at 0x6A00 with a 16 GiB step; k is assigned by lockfile topology
/// order. Stack = [base][img_k…][delta]; absolute addresses across regions are mutually
/// stable (cacheability criterion ① holds for every region).
pub const IMAGE_SPLINE_BASE: usize = 0x6A00_0000_0000;
pub const IMAGE_SPLINE_STEP: usize = 1 << 34;
pub const IMAGE_SPLINE_COUNT: usize = 1300;

// ---- Three code-region families (P1 entry stubs: fn-ptr values made executable; isomorphic to but disjoint from the three frozen regions) ----

/// Fixed base of the delta code region.
pub const DELTA_CODE_ADDR: usize = 0x6C00_0000_0000;
/// Fixed base of the base code region.
pub const BASE_CODE_ADDR: usize = 0x6D00_0000_0000;
/// Dependency image code-region spline (k uses the same assignment order as the frozen spline).
pub const IMAGE_CODE_SPLINE: usize = 0x6E00_0000_0000;
pub const IMAGE_CODE_STEP: usize = 1 << 34;
pub const IMAGE_CODE_COUNT: usize = 1300;

/// The shadow instruction-pointer base a frame falls back to when no symbol image names it.
///
/// A token address must be one no mapping can occupy, or a backtrace could present it as real code.
/// This base is non-canonical on x86_64 — bits 63..47 are not the sign extension of bit 47 — so the
/// kernel refuses to map it, and the band below it belongs to no region this layout places.
pub const FUNC_IP_BASE: u64 = 0x5f5f_0000_0000_0000;
