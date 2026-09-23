//! The GAS text an x86_64 assembler reads.
//!
//! Only the vocabulary that is the CPU's lives here: which syntax directive wraps a body, how the
//! assembler is asked for the unaligned packed forms below, and the two instruction forms mirvm
//! rewrites into. *How* those pieces are sequenced stays with the materializer in `src/lower/`,
//! because the sequence follows rustc's `prefix_and_suffix` and its bytes are a cache key — a
//! second architecture supplies the same names with its own text and leaves the sequencing alone.

/// The architecture as rustc spells it in [`rustc_target::asm::InlineAsmArch`]'s `Debug`, for a
/// refusal message that names what this build does support.
pub const NAME: &str = "x86_64";

/// Intel syntax. Every body mirvm emits is written in it, which is why the AT&T form is the one
/// that has to be restored rather than the other way round.
pub const DIRECTIVE_INTEL: &str = ".intel_syntax noprefix\n";

/// AT&T syntax, for a body that switched itself.
pub const DIRECTIVE_ATT: &str = ".att_syntax\n";

/// The syntax directive a site carries, preceded by a newline so it cannot continue whatever came
/// before it. Each asm site is independent, so a site cannot contaminate the next one.
pub fn syntax_prefix(att: bool) -> String {
    format!("\n{}", if att { DIRECTIVE_ATT } else { DIRECTIVE_INTEL })
}
