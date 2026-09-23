//! The fixed-address layout the engine's cacheability model asks for: its structure.
//!
//! Absolute addresses have to be stable across processes, because that is what keeps the IR cache
//! working. What the engine asks of every pair is a shape: three frozen data regions — base image,
//! delta, and a spline of dependency images — plus three isomorphic, disjoint code-region families
//! (P1/P2/S3'/S4 address models, decision-history §7.5b/§7.6/§7.5c/§7.3). The shape, and every
//! predicate over it, is the same on every pair, so it is declared here.
//!
//! The numbers are the pair's, because they are chosen against one kernel's address space: where
//! this CPU running this kernel leaves a hole wide enough for the regions, and which band above the
//! user address space is guaranteed unmappable for a frame's fallback instruction-pointer token.
//! They live in the pair directory and are re-exported below, so a caller reads a base and its
//! whitelist from one name.
//!
//! Arena implementations in `vm/frozen.rs` and `vm/codearena.rs` use this as the source of truth;
//! the IR serialization whitelists, the image layers' load validation, and lower assembly all share
//! the same numeric values; no second literal is allowed.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) use super::linux_x86_64::addrspace::*;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use super::macos_aarch64::addrspace::*;

/// Fixed-region base for the k-th dependency image.
pub(crate) fn image_addr(k: usize) -> usize {
    assert!(k < IMAGE_SPLINE_COUNT, "image spline out of bounds: k={k}");
    IMAGE_SPLINE_BASE + k * IMAGE_SPLINE_STEP
}

/// Valid frozen-region whitelist: base / delta / dependency image splines (aligned and in bounds).
/// Both restore and serde deserialization go through it — prevents forged snapshots from
/// placing regions at arbitrary addresses (wrong base = silent wrong values).
pub(crate) fn is_valid_home(addr: usize) -> bool {
    addr == BASE_IMAGE_FIXED_ADDR
        || addr == DELTA_FIXED_ADDR
        || (addr >= IMAGE_SPLINE_BASE
            && (addr - IMAGE_SPLINE_BASE).is_multiple_of(IMAGE_SPLINE_STEP)
            && (addr - IMAGE_SPLINE_BASE) / IMAGE_SPLINE_STEP < IMAGE_SPLINE_COUNT)
}

/// Code-region base for the k-th dependency image.
pub(crate) fn image_code_addr(k: usize) -> usize {
    assert!(
        k < IMAGE_CODE_COUNT,
        "image code spline out of bounds: k={k}"
    );
    IMAGE_CODE_SPLINE + k * IMAGE_CODE_STEP
}

/// Frozen-region base -> this module's code-region base (same-k invariant: delta↔delta,
/// base↔base, image_spline(k)↔image_code(k)). Non-whitelisted frozen base => None
/// (engine invariant violation).
pub(crate) fn code_home_for_frozen(home: usize) -> Option<usize> {
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
pub(crate) fn is_valid_code_home(addr: usize) -> bool {
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
