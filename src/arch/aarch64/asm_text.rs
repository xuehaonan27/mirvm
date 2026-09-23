//! The GAS text an aarch64 assembler reads.
//!
//! This architecture has one syntax, so where x86_64 has two directives that switch between them
//! there is nothing to switch. The constants stay because the materializer in `src/lower/` names
//! them on every architecture — its sequence is shared, and an architecture supplies what its own
//! assembler needs, which here is nothing. *How* the pieces are sequenced also stays there, because
//! the sequence follows rustc's `prefix_and_suffix` and its bytes are a cache key.

/// The architecture as rustc spells it in [`rustc_target::asm::InlineAsmArch`]'s `Debug`, for a
/// refusal message that names what this build does support.
pub const NAME: &str = "aarch64";

/// Empty: this architecture's assembler has a single syntax, so a body never switches into one.
pub const DIRECTIVE_INTEL: &str = "";

/// Empty, for the same reason as [`DIRECTIVE_INTEL`].
pub const DIRECTIVE_ATT: &str = "";

/// The separator a site carries, so a site cannot continue whatever came before it. There is no
/// directive to add here, which is why this is only the newline.
pub fn syntax_prefix(_att: bool) -> String {
    "\n".to_string()
}
