//! The 128-bit conversions no host instruction performs.
//!
//! A conversion between `u128`/`i128` and a scalar float is a libcall on every platform this
//! build targets, and one of those platforms does not carry the call at all: the runtime symbol
//! for `f16` (`__floattihf`/`__floatuntihf`) is missing from the `aarch64-apple-darwin` runtime,
//! so a native program whose `u128 as f16` runs at runtime fails to link there too. The `f16`
//! conversion is therefore modelled here, for both backends to share, so that the answer does not
//! depend on which runtime the build happened to link.
//!
//! The two narrower conversions need no model: `f32` and `f64` have their own instructions on both
//! platforms, and the backends take those directly.

/// The sign bit of `f16`.
const F16_SIGN: u16 = 0x8000;
/// The bit pattern of `f16` infinity, which is what a magnitude past the format's range saturates
/// to rather than wrapping.
const F16_INFINITY: u16 = 0x7c00;
/// The significand's stored width in `f16`, and the value of its implicit bit.
const F16_MANTISSA_BITS: u32 = 10;
const F16_IMPLICIT: u32 = 1 << F16_MANTISSA_BITS;
/// `f16`'s exponent bias, and the biased value that means infinity or a NaN.
const F16_BIAS: i32 = 15;
const F16_MAX_BIASED: i32 = 0x1f;

/// `u128`/`i128` → `f16`, as the bit pattern of the result.
///
/// `signed` reads the value as two's complement, which is how the guest's own cast reads it: a
/// negative value keeps its sign and its magnitude is rounded, so a tie rounds to even on the
/// magnitude either way.
pub(crate) fn wide_to_f16_bits(value: u128, signed: bool) -> u16 {
    let (sign, magnitude) = if signed && value >> 127 == 1 {
        (F16_SIGN, value.wrapping_neg())
    } else {
        (0, value)
    };
    sign | magnitude_to_f16(magnitude)
}

/// A magnitude's `f16` bit pattern: round to nearest, ties to even.
///
/// The magnitude is a non-negative integer, so the subnormal range is out of reach — the smallest
/// value reaching here is 1 — and the whole conversion is one rounding of an 11-bit significand.
fn magnitude_to_f16(magnitude: u128) -> u16 {
    if magnitude == 0 {
        return 0;
    }
    // The highest set bit is the value's binary exponent, since the value is an integer.
    let exponent = (127 - magnitude.leading_zeros()) as i32;
    if exponent <= F16_MANTISSA_BITS as i32 {
        // Every integer below 2^11 is exact: an 11-bit significand reaches that far, and the
        // implicit bit is the one the value's highest set bit stands for. What is left over is the
        // fraction, which is scaled up to the significand's own width.
        let mantissa = (magnitude - (1u128 << exponent)) << (F16_MANTISSA_BITS as i32 - exponent);
        return (((exponent + F16_BIAS) as u16) << F16_MANTISSA_BITS) | mantissa as u16;
    }
    // Keep the 11 bits the format carries and decide the last one from what is dropped.
    let shift = (exponent - F16_MANTISSA_BITS as i32) as u32;
    let kept = (magnitude >> shift) as u32;
    let dropped = magnitude & ((1u128 << shift) - 1);
    let half = 1u128 << (shift - 1);
    let round_up = dropped > half || (dropped == half && kept & 1 == 1);
    let (exponent, kept) = if kept + u32::from(round_up) == F16_IMPLICIT * 2 {
        // The carry moved the point up one place and the significand restarts at the implicit bit.
        (exponent + 1, F16_IMPLICIT)
    } else {
        (exponent, kept + u32::from(round_up))
    };
    let biased = exponent + F16_BIAS;
    if biased >= F16_MAX_BIASED {
        return F16_INFINITY;
    }
    ((biased as u16) << F16_MANTISSA_BITS) | (kept as u16 & 0x3ff)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact integers: everything below 2^11 is representable, and the values around the
    /// format's limits are where a model goes wrong most easily.
    #[test]
    fn small_integers_are_exact() {
        for (value, bits) in [
            (0_u128, 0x0000_u16),
            (1, 0x3c00),
            (2, 0x4000),
            (1023, 0x63fe),
            (1024, 0x6400),
            (1025, 0x6401),
            (2047, 0x67ff),
            (2048, 0x6800),
        ] {
            assert_eq!(wide_to_f16_bits(value, false), bits, "{value}");
        }
    }

    /// Rounding: a dropped bit above the halfway point rounds up, exactly halfway rounds to the
    /// even significand, and a carry out of the significand moves the exponent.
    #[test]
    fn rounding_is_to_nearest_and_ties_to_even() {
        // 2049 is halfway between 2048 and 2050, and 2048's significand is even.
        assert_eq!(wide_to_f16_bits(2049, false), 0x6800);
        // 2051 is halfway between 2050 and 2052, and 2052's significand is even.
        assert_eq!(wide_to_f16_bits(2051, false), 0x6802);
        // 2050 is neither a tie nor rounded away.
        assert_eq!(wide_to_f16_bits(2050, false), 0x6801);
        // The carry out of the significand: 65520 is the largest value that rounds up to the
        // format's limit, and the tie it sits on pushes it past.
        assert_eq!(wide_to_f16_bits(65504, false), 0x7bff);
        assert_eq!(wide_to_f16_bits(65519, false), 0x7bff);
        assert_eq!(wide_to_f16_bits(65520, false), F16_INFINITY);
    }

    /// Everything past the format's range saturates to infinity, which the libcall does and a
    /// wrapping encoder would not.
    #[test]
    fn a_magnitude_past_the_range_saturates() {
        assert_eq!(wide_to_f16_bits(u128::MAX, false), F16_INFINITY);
        assert_eq!(wide_to_f16_bits(1_u128 << 100, false), F16_INFINITY);
        assert_eq!(wide_to_f16_bits(i128::MIN as u128, true), 0xfc00);
        assert_eq!(wide_to_f16_bits(-1_i128 as u128, true), 0xbc00);
        // Read as two's complement this is -1, not the largest magnitude.
        assert_eq!(wide_to_f16_bits(u128::MAX, true), 0xbc00);
    }
}
