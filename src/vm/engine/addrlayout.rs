//! Fixed-base address layout (P1/P2/S3'/S4 address models, decision-history §7.5b/§7.6/§7.5c/§7.3):
//! the foundational constant layer for engine cacheability — all fixed bases/spline
//! parameters and whitelist criteria for the three frozen data regions and the three
//! code-region families. Arena implementations in frozen.rs/codearena.rs use this as
//! the source of truth; IR serialization whitelists, baseimage/depsimage load
//! validation, and lower assembly all share the same numeric values; no second literal
//! is allowed.
//!
//! Site-selection rationale (Linux x86_64 virtual address space knowledge, same
//! premise as the global_asm/asm-stub x86_64 hard floor): PIE image/brk randomization
//! upper bound ~0x66xx_xxxx_xxxx (mmap_rnd_bits=28), the top-down mmap region is near
//! 0x7fxx_xxxx_xxxx — the 0x68–0x6E band falls in the hole between the two and does
//! not intersect the shadow IP region (FUNC_IP_BASE, non-canonical high bits, never
//! mapped). The spline step of 16 GiB is far larger than each region's capacity (the
//! hole allows future expansion), and the upper bound of 1300 stays below 0x7f.

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

/// Fixed-region base for the k-th dependency image.
pub fn image_addr(k: usize) -> usize {
    assert!(k < IMAGE_SPLINE_COUNT, "image spline out of bounds: k={k}");
    IMAGE_SPLINE_BASE + k * IMAGE_SPLINE_STEP
}

/// Valid frozen-region whitelist: base / delta / dependency image splines (aligned and in bounds).
/// Both restore and serde deserialization go through it — prevents forged snapshots from
/// placing regions at arbitrary addresses (wrong base = silent wrong values).
pub fn is_valid_home(addr: usize) -> bool {
    addr == BASE_IMAGE_FIXED_ADDR
        || addr == DELTA_FIXED_ADDR
        || (addr >= IMAGE_SPLINE_BASE
            && (addr - IMAGE_SPLINE_BASE).is_multiple_of(IMAGE_SPLINE_STEP)
            && (addr - IMAGE_SPLINE_BASE) / IMAGE_SPLINE_STEP < IMAGE_SPLINE_COUNT)
}

// ---- Three code-region families (P1 entry stubs: fn-ptr values made executable; isomorphic to but disjoint from the three frozen regions) ----

/// Fixed base of the delta code region.
pub const DELTA_CODE_ADDR: usize = 0x6C00_0000_0000;
/// Fixed base of the base code region.
pub const BASE_CODE_ADDR: usize = 0x6D00_0000_0000;
/// Dependency image code-region spline (k uses the same assignment order as the frozen spline).
pub const IMAGE_CODE_SPLINE: usize = 0x6E00_0000_0000;
pub const IMAGE_CODE_STEP: usize = 1 << 34;
pub const IMAGE_CODE_COUNT: usize = 1300;

/// Code-region base for the k-th dependency image.
pub fn image_code_addr(k: usize) -> usize {
    assert!(
        k < IMAGE_CODE_COUNT,
        "image code spline out of bounds: k={k}"
    );
    IMAGE_CODE_SPLINE + k * IMAGE_CODE_STEP
}

/// Frozen-region base -> this module's code-region base (same-k invariant: delta↔delta,
/// base↔base, image_spline(k)↔image_code(k)). Non-whitelisted frozen base => None
/// (engine invariant violation).
pub fn code_home_for_frozen(home: usize) -> Option<usize> {
    match home {
        DELTA_FIXED_ADDR => Some(DELTA_CODE_ADDR),
        BASE_IMAGE_FIXED_ADDR => Some(BASE_CODE_ADDR),
        h if h >= IMAGE_SPLINE_BASE
            && (h - IMAGE_SPLINE_BASE).is_multiple_of(IMAGE_SPLINE_STEP)
            && (h - IMAGE_SPLINE_BASE) / IMAGE_SPLINE_STEP < IMAGE_SPLINE_COUNT =>
        {
            Some(image_code_addr((h - IMAGE_SPLINE_BASE) / IMAGE_SPLINE_STEP))
        }
        _ => None,
    }
}

/// Valid code-region whitelist (double-checked during serde recipe replay and load defense; forged snapshot guard).
pub fn is_valid_code_home(addr: usize) -> bool {
    addr == DELTA_CODE_ADDR
        || addr == BASE_CODE_ADDR
        || (addr >= IMAGE_CODE_SPLINE
            && (addr - IMAGE_CODE_SPLINE).is_multiple_of(IMAGE_CODE_STEP)
            && (addr - IMAGE_CODE_SPLINE) / IMAGE_CODE_STEP < IMAGE_CODE_COUNT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas_and_whitelist() {
        assert!(is_valid_home(BASE_IMAGE_FIXED_ADDR));
        assert!(is_valid_home(DELTA_FIXED_ADDR));
        assert!(is_valid_home(image_addr(0)));
        assert!(is_valid_home(image_addr(IMAGE_SPLINE_COUNT - 1)));
        assert!(!is_valid_home(image_addr(IMAGE_SPLINE_COUNT - 1) + 0x1000));
        assert_eq!(
            code_home_for_frozen(DELTA_FIXED_ADDR),
            Some(DELTA_CODE_ADDR)
        );
        assert_eq!(
            code_home_for_frozen(BASE_IMAGE_FIXED_ADDR),
            Some(BASE_CODE_ADDR)
        );
        assert_eq!(
            code_home_for_frozen(image_addr(7)),
            Some(image_code_addr(7))
        );
        assert!(code_home_for_frozen(0x1234_5678_0000).is_none());
        assert!(is_valid_code_home(DELTA_CODE_ADDR));
        assert!(is_valid_code_home(image_code_addr(IMAGE_CODE_COUNT - 1)));
        assert!(!is_valid_code_home(DELTA_FIXED_ADDR));
    }
}
