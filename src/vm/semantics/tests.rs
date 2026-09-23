//! The modelled 128-bit conversion, held to the host's own where the host has one.
//!
//! [`super::wide`] models `u128`/`i128` → `f16` because the runtime symbol the host conversion
//! would call is not present on every platform this build supports. The platform that *does* have
//! the symbol is where the model can be checked against it, which is why this comparison is here
//! rather than beside the model: the host's cast is the authority and it is the half that is
//! missing on the other side.

/// The values a rounding model goes wrong on: every power of two with its neighbours, the extremes,
/// and a value sitting exactly on the halfway point at every reachable shift.
#[cfg(target_os = "linux")]
fn probes() -> Vec<u128> {
    let mut values = vec![0, 1, 2, 3, u128::MAX, i128::MIN as u128, u128::MAX / 3];
    for bit in 0..128 {
        let power = 1u128 << bit;
        values.extend([
            power,
            power.wrapping_sub(1),
            power.wrapping_add(1),
            power | 1,
        ]);
        if bit >= 11 {
            // An 11-bit significand with the bit below it set and nothing else: exactly a tie.
            values.push(power | (1 << (bit - 11)));
        }
    }
    values
}

#[cfg(target_os = "linux")]
#[test]
fn the_f16_model_matches_the_hosts_conversion() {
    for value in probes() {
        assert_eq!(
            super::wide::wide_to_f16_bits(value, false),
            (value as f16).to_bits(),
            "u128 {value:#x}"
        );
        let signed = value as i128;
        assert_eq!(
            super::wide::wide_to_f16_bits(value, true),
            (signed as f16).to_bits(),
            "i128 {signed:#x}"
        );
        let negated = signed.wrapping_neg() as u128;
        assert_eq!(
            super::wide::wide_to_f16_bits(negated, true),
            (signed.wrapping_neg() as f16).to_bits(),
            "i128 {:#x}",
            signed.wrapping_neg()
        );
    }
}
