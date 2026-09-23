//! The numbers of this pair's fixed-address layout.
//!
//! The structure and every predicate over it are [`crate::os_arch::addrspace`]'s, because they hold
//! for every pair. What is here is the one thing that does not: where macOS on aarch64 leaves a
//! hole wide enough for the three frozen data regions and the three code-region families.
//!
//! Site-selection rationale (macOS aarch64 virtual address space knowledge, measured on the
//! development machine). User space ends at 47 bits: an address with bit 47 set is refused, bit 46
//! still maps, so the whole layout must fit below 0x8000_0000_0000. dyld and the C library occupy
//! only the bottom of that range — the main image near 0x1_0000_0000, the shared cache and the
//! system libraries from 0x1_8000_0000 upward, thread stacks near 0x1_6f00_0000 rather than at the
//! top of the address space, and a fresh `mmap(NULL)` just below the main image — so everything the
//! system hands out stays under roughly 12 GiB. The 0x68–0x6E band, which starts at
//! 0x68_0000_0000_00, therefore sits four orders of magnitude above every system region and still
//! far below the 47-bit ceiling. All five bases were verified free there.
//!
//! The spline step of 16 GiB is far larger than each region's capacity, and the upper bound of 1300
//! keeps the last region below 0x7f, so no region can reach the ceiling either. Should a future
//! system layout grow into this band, the fixed mapping simply fails and the arena falls back to a
//! dynamic base — which costs serializability and nothing else.

// ---- Three frozen data regions (data: statics/constant pool/fn-ptr entries) ----

/// Fixed base of the base image region (std pre-lowered modules shared across programs).
pub const BASE_IMAGE_FIXED_ADDR: usize = 0x6800_0000_0000;
/// Fixed base of the delta region (this program's modules; equals the full set when no base image exists).
pub const DELTA_FIXED_ADDR: usize = 0x6900_0000_0000;

/// Dependency image region spline: each registry dependency image occupies one fixed region,
/// starting at 0x6A00 with a 16 GiB step; k is assigned by lockfile topology order. Stack =
/// [base][img_k…][delta]; absolute addresses across regions are mutually stable.
pub const IMAGE_SPLINE_BASE: usize = 0x6A00_0000_0000;
pub const IMAGE_SPLINE_STEP: usize = 1 << 34;
pub const IMAGE_SPLINE_COUNT: usize = 1300;

// ---- Three code-region families (entry stubs: fn-ptr values made executable) ----

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
/// User space here stops at 47 bits, so any address above that ceiling is unmappable: this base has
/// bits 48 and above set, and the kernel refuses it.
pub const FUNC_IP_BASE: u64 = 0x5f5f_0000_0000_0000;
