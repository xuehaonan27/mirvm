//! The standard C math symbols compiled code imports.
//!
//! The names are the C standard's, and `translate/value.rs` spells the same ones when it declares an
//! import: the width picks the `f` suffix. The interpreter does not come through here — it calls the
//! Rust methods (`x.sqrt()`) that rustc lowers to the same host symbols — so this table is only the
//! address each name resolves to, handed to the JITBuilder, and the differential suite is what keeps
//! the two channels agreeing.
//!
//! One list writes both halves: the declaration that takes the address and the table that hands it
//! over. Taking an address is all mirvm needs, so the functions are declared rather than called.

/// The libc crate no longer binds the math functions, so they are declared here to take their
/// addresses; the process already links them.
macro_rules! math_symbols {
    ($( $name:ident ),* $(,)?) => {
        mod decls {
            unsafe extern "C" {
                $( pub fn $name(); )*
            }
        }

        /// Every symbol the translator may name, as `(name, address)`.
        pub(crate) fn math_symbols() -> Vec<(&'static str, usize)> {
            vec![ $( (stringify!($name), decls::$name as *const () as usize) ),* ]
        }
    };
}

math_symbols!(
    sqrtf, sqrt, sinf, sin, cosf, cos, expf, exp, exp2f, exp2, logf, log, log2f, log2, log10f,
    log10, fabsf, fabs, floorf, floor, ceilf, ceil, truncf, trunc, roundf, round, rintf, rint,
    powf, pow, copysignf, copysign, fminf, fmin, fmaxf, fmax, fmodf, fmod,
);
