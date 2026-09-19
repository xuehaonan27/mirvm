//! Cranelift ISA construction for the two code domains.

use super::*;

/// Flags for the plain domain. Kept separate from [`trace_domain_flags`] so the two
/// domains are visibly identical apart from the one setting the trace domain adds.
fn plain_domain_flags() -> settings::Flags {
    let mut fb = settings::builder();
    fb.set("opt_level", "speed").unwrap();
    fb.set("unwind_info", "true").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    settings::Flags::new(fb)
}

/// ISA flags for the trace code domain.
///
/// The trace domain is a separate Cranelift ISA/module from the plain domain:
/// it enables the pinned register so trace code can hold the current thread's
/// recorder state in `r15` and reach the inline syscall path without a TLS
/// lookup or a global session check. On x86-64 Cranelift's pinned register is
/// exactly `r15` (`isa/x64/inst/regs.rs`, documented there as matching
/// Spidermonkey's HeapReg).
///
/// The setting is ISA-wide, so the trace domain cannot be a flag on the plain
/// ISA: plain code must keep `r15` allocatable and carry no collection state.
/// This constructor is the single home for that separation.
pub(crate) fn trace_domain_flags() -> settings::Flags {
    let mut fb = settings::builder();
    fb.set("opt_level", "speed").unwrap();
    fb.set("unwind_info", "true").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    // The one deliberate delta from the plain domain: the pinned register is
    // what makes `get_pinned_reg`/`set_pinned_reg` meaningful, and without it
    // Cranelift rejects those instructions.
    fb.set("enable_pinned_reg", "true").unwrap();
    settings::Flags::new(fb)
}

/// ISA for a domain: the plain flags, or the same flags plus the pinned register.
pub(crate) fn domain_isa(domain: CodeDomain) -> cranelift_codegen::isa::OwnedTargetIsa {
    let flags = match domain {
        CodeDomain::Plain => plain_domain_flags(),
        CodeDomain::Trace => trace_domain_flags(),
    };
    cranelift_native::builder()
        .expect("native ISA builder")
        .finish(flags)
        .expect("native ISA accepts the domain flags")
}

/// Build the trace domain's ISA. Only tests call this today, hence the `dead_code`
/// allowance.
#[allow(dead_code)]
pub(crate) fn trace_domain_isa() -> cranelift_codegen::isa::OwnedTargetIsa {
    domain_isa(CodeDomain::Trace)
}
