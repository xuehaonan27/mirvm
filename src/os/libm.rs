//! The C library's math surface, as symbol names.
//!
//! The interpreter calls these through the host linker and the JIT registers them as import
//! symbols, so the names have to be the platform's: `sqrt`/`sqrtf` and their family. Taking an
//! address is all mirvm needs, which is why they are declared rather than called from here.

/// The symbol table the JITBuilder is given, holding the same symbols the interpreter's host
/// calls use. The libc crate no longer binds the math functions, so they are declared to take
/// their addresses; the process already links them.
mod decls {
    unsafe extern "C" {
        pub fn sqrtf();
        pub fn sqrt();
        pub fn sinf();
        pub fn sin();
        pub fn cosf();
        pub fn cos();
        pub fn expf();
        pub fn exp();
        pub fn exp2f();
        pub fn exp2();
        pub fn logf();
        pub fn log();
        pub fn log2f();
        pub fn log2();
        pub fn log10f();
        pub fn log10();
        pub fn fabsf();
        pub fn fabs();
        pub fn floorf();
        pub fn floor();
        pub fn ceilf();
        pub fn ceil();
        pub fn truncf();
        pub fn trunc();
        pub fn roundf();
        pub fn round();
        pub fn rintf();
        pub fn rint();
        pub fn powf();
        pub fn pow();
        pub fn copysignf();
        pub fn copysign();
        pub fn fminf();
        pub fn fmin();
        pub fn fmaxf();
        pub fn fmax();
        pub fn fmodf();
        pub fn fmod();
    }
}
pub fn symbols() -> Vec<(&'static str, usize)> {
    vec![
        ("sqrtf", decls::sqrtf as *const () as usize),
        ("sqrt", decls::sqrt as *const () as usize),
        ("sinf", decls::sinf as *const () as usize),
        ("sin", decls::sin as *const () as usize),
        ("cosf", decls::cosf as *const () as usize),
        ("cos", decls::cos as *const () as usize),
        ("expf", decls::expf as *const () as usize),
        ("exp", decls::exp as *const () as usize),
        ("exp2f", decls::exp2f as *const () as usize),
        ("exp2", decls::exp2 as *const () as usize),
        ("logf", decls::logf as *const () as usize),
        ("log", decls::log as *const () as usize),
        ("log2f", decls::log2f as *const () as usize),
        ("log2", decls::log2 as *const () as usize),
        ("log10f", decls::log10f as *const () as usize),
        ("log10", decls::log10 as *const () as usize),
        ("fabsf", decls::fabsf as *const () as usize),
        ("fabs", decls::fabs as *const () as usize),
        ("floorf", decls::floorf as *const () as usize),
        ("floor", decls::floor as *const () as usize),
        ("ceilf", decls::ceilf as *const () as usize),
        ("ceil", decls::ceil as *const () as usize),
        ("truncf", decls::truncf as *const () as usize),
        ("trunc", decls::trunc as *const () as usize),
        ("roundf", decls::roundf as *const () as usize),
        ("round", decls::round as *const () as usize),
        ("rintf", decls::rintf as *const () as usize),
        ("rint", decls::rint as *const () as usize),
        ("powf", decls::powf as *const () as usize),
        ("pow", decls::pow as *const () as usize),
        ("copysignf", decls::copysignf as *const () as usize),
        ("copysign", decls::copysign as *const () as usize),
        ("fminf", decls::fminf as *const () as usize),
        ("fmin", decls::fmin as *const () as usize),
        ("fmaxf", decls::fmaxf as *const () as usize),
        ("fmax", decls::fmax as *const () as usize),
        ("fmodf", decls::fmodf as *const () as usize),
        ("fmod", decls::fmod as *const () as usize),
    ]
}
