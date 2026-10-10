//! Cranelift ISA construction for the two code domains.

use super::*;

/// Flags for one domain at one tier: one constructor, so the tier is the only thing that can differ
/// between two ISAs this build uses.
///
/// Pinning a register is what can make two domains separate Cranelift ISAs with separate modules
/// rather than one sharing a flag set, because what the register *means* differs and only a
/// domain's own entry points may set it. Trace code always pins the current thread's recorder;
/// plain code pins the current thread's `Ctx` where the pair can carry it across compiled frames
/// ([`crate::os_arch::PLAIN_CODE_PINS_CTX`]), so a compiled entry reads its stack floor with a
/// register read instead of a TLS lookup into `ctx::current`. Which register that is belongs to the
/// architecture ([`crate::arch::PINNED_REG`]), and Cranelift documents the choice in its own
/// register environment.
pub(crate) fn domain_flags(tier: Tier, pin_register: bool) -> settings::Flags {
    let mut fb = settings::builder();
    fb.set("opt_level", tier.opt_level()).unwrap();
    fb.set("unwind_info", "true").unwrap();
    fb.set("preserve_frame_pointers", "true").unwrap();
    if pin_register {
        fb.set("enable_pinned_reg", "true").unwrap();
    }
    settings::Flags::new(fb)
}

/// ISA for a domain at one tier.
pub(crate) fn isa_for(domain: CodeDomain, tier: Tier) -> cranelift_codegen::isa::OwnedTargetIsa {
    let pin = domain == CodeDomain::Trace || crate::os_arch::PLAIN_CODE_PINS_CTX;
    let flags = domain_flags(tier, pin);
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
