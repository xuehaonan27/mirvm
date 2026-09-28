//! Cranelift ISA construction for the two code domains.

use super::*;

/// Flags for one domain at one tier: one constructor, so the tier and the domain are the only two
/// things that can differ between two ISAs this build uses.
///
/// The trace domain is a separate Cranelift ISA/module from the plain domain: it enables the pinned
/// register so trace code can hold the current thread's recorder state in the register Cranelift
/// reserves for it and reach the inline syscall path without a TLS lookup or a global session check.
/// Which register that is belongs to the architecture ([`crate::arch::PINNED_REG`]); Cranelift
/// documents the choice in its own register environment. The setting is ISA-wide, so the trace domain
/// cannot be a flag on the plain ISA: plain code must keep that register allocatable and carry no
/// collection state.
pub(crate) fn domain_flags(tier: Tier, trace: bool) -> settings::Flags {
    let mut fb = settings::builder();
    fb.set("opt_level", tier.opt_level()).unwrap();
    fb.set("unwind_info", "true").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    if trace {
        fb.set("enable_pinned_reg", "true").unwrap();
    }
    settings::Flags::new(fb)
}

/// ISA for a domain at one tier.
pub(crate) fn isa_for(domain: CodeDomain, tier: Tier) -> cranelift_codegen::isa::OwnedTargetIsa {
    let flags = domain_flags(tier, domain == CodeDomain::Trace);
    let mut builder = cranelift_native::builder().expect("native ISA builder");
    // The host's own flags would have every frame description claim its return address is signed,
    // which is a promise the code has to keep: the unwinder authenticates what it is told to, and a
    // promise made about code that does not make it is worse than no promise. mirvm's bodies are
    // its own, so it declines the two flags rather than adopting an ABI it does not implement.
    // Measured, this is what a frame under this platform's unwinder needs.
    for flag in ["sign_return_address", "sign_return_address_with_bkey"] {
        // A platform without the flag has nothing to decline.
        let _ = builder.set(flag, "false");
    }
    builder.finish(flags).unwrap_or_else(|error| {
        // The flag this constructor adds is the one most likely to be refused, and it is
        // about exactly one register, so the message names it.
        panic!(
            "native ISA rejects the domain flags (pinned register {}): {error}",
            crate::arch::PINNED_REG
        )
    })
}

/// ISA for a domain at the optimized tier: what the probe tests and the trace boundary build with.
pub(crate) fn domain_isa(domain: CodeDomain) -> cranelift_codegen::isa::OwnedTargetIsa {
    isa_for(domain, Tier::Optimized)
}

/// Build the trace domain's ISA. Only tests call this today, hence the `dead_code`
/// allowance.
#[allow(dead_code)]
pub(crate) fn trace_domain_isa() -> cranelift_codegen::isa::OwnedTargetIsa {
    domain_isa(CodeDomain::Trace)
}
