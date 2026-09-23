//! Kernel normalization: the action the kernel accepted versus the request.

use super::*;

extern "C" fn second_test_restorer() {}

// The flags this test drives are the Linux kernel's: it is the one that clears a probing bit
// on the way back out, and the only one whose action carries a restorer to change.
#[cfg(target_os = "linux")]
#[test]
fn kernel_normalization_rejects_restorer_and_supported_flag_changes() {
    let mut requested = Sigaction::empty(
        0x1_1000,
        SA_RESTART | RESTORER_FLAG | SA_UNSUPPORTED | SA_EXPOSE_TAGBITS,
    );
    requested.set_restorer(first_test_restorer);

    let mut accepted = requested;
    accepted.clear_flags(SA_UNSUPPORTED);
    assert!(accepted.is_kernel_normalization_of(&requested));

    let mut wrong_restorer = accepted;
    wrong_restorer.set_restorer(second_test_restorer);
    assert!(!wrong_restorer.is_kernel_normalization_of(&requested));

    let mut missing_supported = accepted;
    missing_supported.clear_flags(SA_EXPOSE_TAGBITS);
    assert!(!missing_supported.is_kernel_normalization_of(&requested));
}
